//! The commands a person runs once: `setup` (the hub), `invite`, `join` (everyone else), `doctor`.

use crate::client::Client;
use crate::config::{home, Config, HubConfig, PeerConfig, DEFAULT_PORT, VERSION};
use crate::{agents, hub, service, tailcat};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

fn ok(s: impl AsRef<str>) {
    println!("  \x1b[32m✓\x1b[0m {}", s.as_ref());
}

fn warn(s: impl AsRef<str>) {
    println!("  \x1b[33m!\x1b[0m {}", s.as_ref());
}

fn bad(s: impl AsRef<str>) {
    println!("  \x1b[31m✗\x1b[0m {}", s.as_ref());
}

fn default_name() -> String {
    let u = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_else(|_| "me".into());
    let s: String = u.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    if s.is_empty() { "me".into() } else { s }
}

pub struct Options {
    pub name: Option<String>,
    pub port: Option<u16>,
    pub no_tailcat: bool,
    pub no_service: bool,
    pub no_agents: bool,
}

/// Wait until the hub answers `whoami` with this config's token.
fn wait_for_hub(cfg: &Config, secs: u64) -> Result<Value> {
    let c = Client::new(&cfg.url, &cfg.token, &cfg.name);
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match c.whoami() {
            Ok(v) => return Ok(v),
            Err(e) if Instant::now() >= deadline => return Err(e),
            Err(_) => std::thread::sleep(Duration::from_millis(500)),
        }
    }
}

fn finish_install(cfg: &Config, o: &Options, role_word: &str) -> Result<()> {
    let exe = service::install_binary()?;
    ok(format!("binary installed at {}", exe.display()));
    if o.no_service {
        warn(format!("no background service (--no-service): run `{} daemon` yourself", exe.display()));
    } else {
        let what = service::install(&exe)?;
        ok(format!("background service: {what}"));
        let who = wait_for_hub(cfg, 60).with_context(|| {
            format!("the {role_word} service started but the hub does not answer; see {}",
                    service::log_path().display())
        })?;
        ok(format!("hub answers: you are {} (version {})", who["as"].as_str().unwrap_or("?"),
                   who["version"].as_str().unwrap_or("?")));
    }
    if o.no_agents {
        warn("agents not registered (--no-agents): run `switchboard agents install` later");
    } else {
        for line in agents::install(&exe)? {
            ok(line);
        }
    }
    Ok(())
}

/// Make this machine the hub.
pub fn setup(o: Options) -> Result<()> {
    println!("\x1b[1mswitchboard {VERSION}: setting up this machine as the hub\x1b[0m");
    let existing = Config::load().ok();
    if let Some(c) = &existing {
        if c.role == "peer" {
            bail!("this machine already joined a hub as {}; it can't also be a hub (use another SWITCHBOARD_HOME)",
                  c.name);
        }
    }
    let name = o.name.clone().or_else(|| existing.as_ref().map(|c| c.name.clone())).unwrap_or_else(default_name);
    if !crate::config::valid_box(&name) || name.contains('/') {
        bail!("--name must be a plain name like `sean`, not {name:?}");
    }
    let mut hc = existing.as_ref().and_then(|c| c.hub.clone()).unwrap_or_default();
    if let Some(p) = o.port {
        hc.port = p;
    }
    hc.tailcat = !o.no_tailcat;
    let tailcat_bin = if o.no_tailcat {
        None
    } else {
        let bin = tailcat::find_or_install(false)?;
        ok(format!("tailcat found at {bin}"));
        tailcat::ensure_server_key(&bin, tailcat::HUB_KEY)?;
        tailcat::ensure_server_key(&bin, tailcat::ENROLL_KEY)?;
        ok("tailcat server keys ready (switchboard, switchboard-enroll)");
        Some(bin)
    };
    let store = hub::open_store(&home(), &hc)?;
    let token = match existing.as_ref().filter(|c| !c.token.is_empty()) {
        Some(c) if store.auth(&c.token)?.map(|a| a.admin).unwrap_or(false) => c.token.clone(),
        _ => store.create_token(&format!("hub admin ({name})"), "*", true)?,
    };
    drop(store);
    let cfg = Config {
        role: "hub".into(),
        name: name.clone(),
        url: format!("http://127.0.0.1:{}", hc.port),
        token,
        tailcat: tailcat_bin,
        hub: Some(hc),
        peer: None,
    };
    cfg.save()?;
    ok(format!("config written to {} (you are `{name}`)", crate::config::config_path().display()));
    finish_install(&cfg, &o, "hub")?;
    println!();
    println!("\x1b[1mThe hub is ready.\x1b[0m Next:");
    if cfg.tailcat.is_some() {
        println!("  switchboard invite tony          # prints a one-line join command for tony's machine");
    }
    println!("  switchboard send {name}/test \"hello\" && switchboard inbox --as {name}/test");
    println!("  switchboard doctor               # checks everything, says how to fix what isn't right");
    Ok(())
}

/// The invite code: `sb1.` + base64url of {e: enrollment address, s: secret, h: hub owner}.
pub fn encode_invite(enroll_addr: &str, secret: &str, hub_owner: &str) -> String {
    format!("sb1.{}", B64.encode(json!({"e": enroll_addr, "s": secret, "h": hub_owner}).to_string()))
}

pub fn decode_invite(code: &str) -> Result<(String, String, String)> {
    let body = code.trim().strip_prefix("sb1.").ok_or_else(|| anyhow!("that is not a switchboard invite (sb1.…)"))?;
    let v: Value = serde_json::from_slice(&B64.decode(body).context("the invite code is damaged")?)?;
    let get = |k: &str| v[k].as_str().map(String::from).ok_or_else(|| anyhow!("the invite code is missing {k}"));
    Ok((get("e")?, get("s")?, get("h").unwrap_or_default()))
}

pub fn invite(prefix: &str, ttl_secs: u64) -> Result<()> {
    let cfg = Config::load()?;
    if cfg.role != "hub" {
        bail!("invites are made on the hub machine");
    }
    if cfg.tailcat.is_none() || !cfg.hub.as_ref().map(|h| h.tailcat).unwrap_or(false) {
        bail!("this hub runs without tailcat (--no-tailcat), so no other machine can join it");
    }
    let c = Client::new(&cfg.url, &cfg.token, &cfg.name);
    let v = c.invite(prefix, ttl_secs)?;
    let secret = v["secret"].as_str().ok_or_else(|| anyhow!("hub gave no secret"))?;
    // The hub starts its enrollment listener when an invite opens; its address file appears a moment later.
    let file = tailcat::enroll_addr_file(&home());
    let deadline = Instant::now() + Duration::from_secs(30);
    let addr = loop {
        if let Some(a) = tailcat::read_addr(&file) {
            break a;
        }
        if Instant::now() > deadline {
            bail!("the enrollment listener did not start; check {}", service::log_path().display());
        }
        std::thread::sleep(Duration::from_millis(300));
    };
    let code = encode_invite(&addr, secret, &cfg.name);
    let hours = ttl_secs as f64 / 3600.0;
    println!("Invite for \x1b[1m{prefix}\x1b[0m: single use, expires in {hours:.0}h. On {prefix}'s machine, run:\n");
    println!("  switchboard join {code}\n");
    println!("Or paste this to their coding agent: \"Install switchboard and run `switchboard join {code}`.\"");
    println!("Treat it like a password: it lets one machine in as {prefix}/*.");
    Ok(())
}

pub struct JoinOptions {
    pub name: Option<String>,
    pub local_port: Option<u16>,
    pub no_service: bool,
    pub no_agents: bool,
}

fn free_port(start: u16) -> u16 {
    (start..start + 50).find(|p| std::net::TcpListener::bind(("127.0.0.1", *p)).is_ok()).unwrap_or(start)
}

pub fn join(code: &str, o: JoinOptions) -> Result<()> {
    println!("\x1b[1mswitchboard {VERSION}: joining a hub\x1b[0m");
    if let Ok(c) = Config::load() {
        if c.role == "hub" {
            bail!("this machine is a hub; join from another machine (or another SWITCHBOARD_HOME)");
        }
    }
    let (enroll_addr, secret, owner) = decode_invite(code)?;
    ok(format!("invite read (from {})", if owner.is_empty() { "the hub" } else { &owner }));
    let bin = tailcat::find_or_install(false)?;
    ok(format!("tailcat found at {bin}"));
    let key = tailcat::ensure_client_key(&bin)?;
    ok(format!("this machine's tailcat key: {}…", &key[..key.len().min(24)]));
    println!("  … contacting the hub (this can take a few seconds while tailcat finds a path)");
    let v = tailcat::enroll_remote(&bin, &enroll_addr, &secret)?;
    let prefix = v["prefix"].as_str().unwrap_or_default().to_string();
    let hub_addr = v["hub_addr"].as_str().ok_or_else(|| anyhow!("the hub did not say its address yet; try again"))?;
    ok(format!("enrolled: this machine may act as {prefix} and {prefix}/*"));
    let name = o.name.clone().unwrap_or_else(|| prefix.clone());
    if !crate::config::prefix_allows(&prefix, &name) {
        bail!("--name {name} is outside {prefix}/*");
    }
    let local_port = o.local_port.unwrap_or_else(|| free_port(DEFAULT_PORT));
    let cfg = Config {
        role: "peer".into(),
        name: name.clone(),
        url: format!("http://127.0.0.1:{local_port}"),
        token: v["token"].as_str().unwrap_or_default().into(),
        tailcat: Some(bin),
        hub: None,
        peer: Some(PeerConfig {
            hub_addr: hub_addr.into(),
            hub_port: v["hub_port"].as_u64().unwrap_or(DEFAULT_PORT as u64) as u16,
            local_port,
        }),
    };
    cfg.save()?;
    ok(format!("config written to {} (you are `{name}`)", crate::config::config_path().display()));
    finish_install(&cfg, &Options { name: None, port: None, no_tailcat: false, no_service: o.no_service,
                                    no_agents: o.no_agents }, "peer")?;
    println!();
    println!("\x1b[1mJoined.\x1b[0m Try:");
    if !owner.is_empty() {
        println!("  switchboard send {owner} \"hello from {name}\"");
    }
    println!("  switchboard inbox");
    println!("  switchboard doctor");
    Ok(())
}

/// Runs on the hub, once per connection to the enrollment listener: read the invite secret, answer with a token.
pub fn enroll_stdio() -> Result<()> {
    let peer_key = std::env::var("TAILCAT_PEER_KEY").unwrap_or_default();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        let _ = tx.send(line);
    });
    let reply = (|| -> Result<Value> {
        let line = rx.recv_timeout(Duration::from_secs(30)).map_err(|_| anyhow!("no invite sent"))?;
        let req: Value = serde_json::from_str(line.trim()).context("bad request")?;
        let secret = req["secret"].as_str().ok_or_else(|| anyhow!("no secret"))?;
        let cfg = Config::load()?;
        let c = Client::new(&cfg.url, &cfg.token, &cfg.name);
        let mut v = c.enroll(secret, &peer_key)?;
        v["ok"] = json!(true);
        Ok(v)
    })();
    let out = match reply {
        Ok(v) => v,
        Err(e) => {
            eprintln!("switchboard: enrollment from {peer_key} refused: {e:#}");
            json!({"ok": false, "error": format!("{e:#}")})
        }
    };
    println!("{out}");
    Ok(())
}

pub fn daemon() -> Result<()> {
    let cfg = Config::load()?;
    match cfg.role.as_str() {
        "hub" => hub::serve_forever(&cfg, None, false),
        "peer" => {
            let p = cfg.peer.clone().ok_or_else(|| anyhow!("peer config is missing its [peer] table"))?;
            eprintln!("switchboard {VERSION}: peer daemon for {}", cfg.name);
            tailcat::run_forward(&cfg.tailcat_bin(), &home(), &p.hub_addr, p.local_port, p.hub_port)
        }
        r => bail!("unknown role {r:?} in config"),
    }
}

/// Check everything and say how to fix what's wrong. Returns false when something is broken.
pub fn doctor() -> Result<bool> {
    println!("\x1b[1mswitchboard {VERSION} doctor\x1b[0m  (home {})", home().display());
    let mut healthy = true;
    let cfg = match Config::load() {
        Ok(c) => {
            ok(format!("config: role {}, name {}, hub {}", c.role, c.name, c.url));
            c
        }
        Err(_) => {
            bad("no config. Fix: `switchboard setup` on the hub, or `switchboard join <code>` everywhere else");
            return Ok(false);
        }
    };
    let needs_tailcat = cfg.role == "peer" || cfg.hub.as_ref().map(|h| h.tailcat).unwrap_or(false);
    if needs_tailcat {
        match tailcat::find() {
            Some(t) => ok(format!("tailcat: {t}")),
            None => {
                bad("tailcat not found. Fix: `brew install tailcat`");
                healthy = false;
            }
        }
    }
    let (installed, running, detail) = service::status();
    match (installed, running) {
        (true, true) => ok(format!("service running ({detail})")),
        (true, false) => {
            bad(format!("service installed but not running ({detail}). Fix: `switchboard service install`; log: {}",
                        service::log_path().display()));
            healthy = false;
        }
        _ => warn("no background service installed (fine if you run `switchboard daemon` yourself). \
                   Install: `switchboard service install`"),
    }
    let c = Client::new(&cfg.url, &cfg.token, &cfg.name);
    match c.health() {
        Ok(h) => ok(format!("hub reachable at {} (version {}, up {}s)", cfg.url, h["version"].as_str().unwrap_or("?"),
                            h["uptime_secs"])),
        Err(e) => {
            bad(format!("hub unreachable: {e:#}"));
            if cfg.role == "peer" {
                println!("      Fix: is the hub machine awake and its service running? Is the forward up? \
                          (`switchboard service install`, then check {})", service::log_path().display());
            } else {
                println!("      Fix: `switchboard service install` (or `switchboard daemon`)");
            }
            healthy = false;
        }
    }
    match c.whoami() {
        Ok(w) => {
            let p = w["prefix"].as_str().unwrap_or("");
            let scope = if p == "*" { "may act as any mailbox".to_string() } else { format!("may use {p} and {p}/*") };
            ok(format!("token valid: acting as {}, {scope}{}", w["as"].as_str().unwrap_or(""),
                       if w["admin"].as_bool() == Some(true) { " (admin)" } else { "" }))
        }
        Err(e) if healthy => {
            bad(format!("token refused: {e:#}. Fix: ask the hub for a new invite and `switchboard join` again"));
            healthy = false;
        }
        Err(_) => {}
    }
    if cfg.role == "hub" {
        if let Ok(h) = c.health() {
            let free = h["disk_free_mb"].as_u64().unwrap_or(0);
            let min = cfg.hub.as_ref().map(|h| h.min_free_mb).unwrap_or(0);
            if free < min {
                bad(format!("disk: {free} MB free, under the {min} MB floor: new messages are refused"));
                healthy = false;
            } else {
                ok(format!("disk: {free} MB free; messages {}", h["stats"]["messages"]));
            }
        }
        if cfg.hub.as_ref().map(|h| h.tailcat).unwrap_or(false) {
            match tailcat::read_addr(&tailcat::hub_addr_file(&home())) {
                Some(_) => ok("tailcat address published (peers can reach the hub)"),
                None => {
                    bad("tailcat has not reported an address yet; is the service running?");
                    healthy = false;
                }
            }
            if let Ok(p) = c.peers() {
                let list: Vec<String> = p.as_array().cloned().unwrap_or_default().iter()
                    .map(|x| x["prefix"].as_str().unwrap_or("?").to_string()).collect();
                ok(format!("{} enrolled peer(s){}", list.len(),
                           if list.is_empty() { ". Add one: `switchboard invite NAME`".into() }
                           else { format!(": {}", list.join(", ")) }));
            }
        }
        let bdir = home().join("backups");
        let latest = std::fs::read_dir(&bdir).ok().and_then(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).max());
        match latest {
            Some(p) => ok(format!("latest backup {}", p.display())),
            None => warn("no backup yet (the hub writes one at start and daily)"),
        }
    }
    let (claude, codex) = agents::status();
    match claude {
        Some(true) => ok("Claude Code: switchboard MCP server registered"),
        Some(false) => warn("Claude Code found but switchboard is not registered. Fix: `switchboard agents install`"),
        None => {}
    }
    match codex {
        Some(true) => ok("Codex: switchboard MCP server registered"),
        Some(false) => warn("Codex found but switchboard is not registered. Fix: `switchboard agents install`"),
        None => {}
    }
    if !healthy {
        if let Ok(log) = std::fs::read_to_string(service::log_path()) {
            println!("  last lines of {}:", service::log_path().display());
            for l in log.lines().rev().take(6).collect::<Vec<_>>().into_iter().rev() {
                println!("      {l}");
            }
        }
    }
    println!("{}", if healthy { "\x1b[32mAll good.\x1b[0m" } else { "\x1b[31mSomething needs fixing (see above).\x1b[0m" });
    Ok(healthy)
}

pub fn default_hub_config() -> HubConfig {
    HubConfig::default()
}
