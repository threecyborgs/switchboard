use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use switchboard::client::Client;
use switchboard::config::{home, parse_duration, Config};
use switchboard::store::NewMsg;
use switchboard::{agents, fmt, hub, mcp, onboard, service};

#[derive(Parser)]
#[command(name = "switchboard", version, about = "Durable mail, work queues and locks for AI agents on different \
machines, carried over tailcat.", after_help = "Start here:\n  hub machine:   switchboard setup\n  then:          \
switchboard invite bob\n  other machine: switchboard join <code>\n  anywhere:      switchboard doctor")]
struct Cli {
    /// Mailbox to act as (person/topic). Default: $SWITCHBOARD_AS, $AGENT_MAIL_AS, or this machine's name.
    #[arg(long = "as", global = true)]
    as_: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Make this machine the hub: keys, config, background service, agent registration.
    Setup {
        /// Your name (the mailbox prefix you act under). Default: $USER.
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        /// Local-only hub: no tailcat, no other machines.
        #[arg(long)]
        no_tailcat: bool,
        /// Don't install the background service (run `switchboard daemon` yourself).
        #[arg(long)]
        no_service: bool,
        /// Don't register the MCP server with Claude Code / Codex.
        #[arg(long)]
        no_agents: bool,
    },
    /// On the hub: make a single-use join code for a person (e.g. `bob`).
    Invite {
        name: String,
        /// How long the code stays valid (30m, 24h, 7d).
        #[arg(long, default_value = "24h")]
        ttl: String,
    },
    /// On another machine: join the hub with the code from `invite`.
    Join {
        code: String,
        /// Act as this mailbox by default (must be under the invited name).
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        local_port: Option<u16>,
        #[arg(long)]
        no_service: bool,
        #[arg(long)]
        no_agents: bool,
    },
    /// Check everything and say how to fix what's wrong.
    Doctor,
    /// Run this machine's role in the foreground (what the service runs).
    Daemon,
    /// Run a hub in the foreground.
    Serve {
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        no_tailcat: bool,
    },
    /// Send a message to a mailbox (bob/merge), a person (bob) or a broadcast (bob/*, *).
    Send {
        to: String,
        body: String,
        #[arg(long)]
        subject: Option<String>,
        #[arg(long)]
        reply_to: Option<i64>,
        /// Expire unread after (e.g. 2h).
        #[arg(long)]
        ttl: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Your open and leased messages and unread notices.
    Inbox {
        /// Include finished ones.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Claim the oldest open message under a lease. Prints nothing when the queue is empty.
    Take {
        /// Lease length (default 15m).
        #[arg(long)]
        lease: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Extend your lease on a message.
    Renew {
        id: i64,
        #[arg(long)]
        lease: Option<String>,
    },
    /// Finish a message; a note goes back to the sender as a reply.
    Done {
        id: i64,
        #[arg(long)]
        note: Option<String>,
    },
    /// Give a message you took back to the queue.
    Release { id: i64 },
    /// A message and its thread.
    Show {
        id: i64,
        #[arg(long)]
        json: bool,
    },
    /// What you sent.
    Sent {
        #[arg(long)]
        json: bool,
    },
    /// Messages whose lease ran out too many times.
    Dead {
        #[arg(long)]
        json: bool,
    },
    /// Put a dead or expired message back in its queue.
    Retry { id: i64 },
    /// Print each new message as it arrives (run under a Monitor). Survives hub restarts.
    Watch {
        #[arg(long)]
        json: bool,
        /// Cursor name: two watchers with different names each see everything.
        #[arg(long, default_value = "watch")]
        consumer: String,
    },
    /// Locks with leases and fencing: status | take | renew | release | check | list.
    Lock {
        #[arg(default_value = "status")]
        action: String,
        /// What you hold it for (a branch name); for `check`, the fence.
        what: Option<String>,
        /// Lock name.
        #[arg(long, default_value = "main")]
        name: String,
        #[arg(long)]
        note: Option<String>,
        /// Keep trying until granted.
        #[arg(long)]
        wait: bool,
        /// Lease length (default 10m).
        #[arg(long, default_value = "10m")]
        ttl: String,
        /// Release someone else's lock (admin only).
        #[arg(long)]
        force: bool,
        #[arg(long)]
        fence: Option<i64>,
    },
    /// Who am I and what may I do.
    Whoami,
    /// Hub admin: enrolled machines.
    Peers {
        /// Remove this peer key (it loses tunnel access at once).
        #[arg(long)]
        remove: Option<String>,
    },
    /// Hub admin: tokens (list | create PREFIX | revoke ID).
    Token {
        #[arg(default_value = "list")]
        action: String,
        arg: Option<String>,
        #[arg(long)]
        admin: bool,
    },
    /// Hub admin: copy the old Python agent-mail database in (keeps ids; safe to rerun).
    ImportAgentMail { path: std::path::PathBuf },
    /// Background service: install | uninstall | status.
    Service {
        #[arg(default_value = "status")]
        action: String,
    },
    /// Register (or remove) switchboard with Claude Code and Codex: install | uninstall.
    Agents {
        #[arg(default_value = "install")]
        action: String,
    },
    /// MCP server on stdio (what Claude Code / Codex run).
    Mcp,
    #[command(hide = true)]
    EnrollStdio,
}

fn identity(cli_as: &Option<String>, cfg: &Config) -> String {
    cli_as
        .clone()
        .or_else(|| std::env::var("SWITCHBOARD_AS").ok())
        .or_else(|| std::env::var("AGENT_MAIL_AS").ok())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| cfg.name.clone())
}

fn client(cli_as: &Option<String>) -> Result<Client> {
    let cfg = Config::load_for_client()?;
    Ok(Client::new(&cfg.url, &cfg.token, &identity(cli_as, &cfg)))
}

fn print_json<T: serde::Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn run() -> Result<i32> {
    let cli = Cli::parse();
    let a = &cli.as_;
    match cli.cmd {
        Cmd::Setup { name, port, no_tailcat, no_service, no_agents } => {
            onboard::setup(onboard::Options { name, port, no_tailcat, no_service, no_agents })?
        }
        Cmd::Invite { name, ttl } => onboard::invite(&name, parse_duration(&ttl)?)?,
        Cmd::Join { code, name, local_port, no_service, no_agents } => {
            onboard::join(&code, onboard::JoinOptions { name, local_port, no_service, no_agents })?
        }
        Cmd::Doctor => return Ok(if onboard::doctor()? { 0 } else { 1 }),
        Cmd::Daemon => onboard::daemon()?,
        Cmd::Serve { port, no_tailcat } => {
            let cfg = Config::load().unwrap_or_else(|_| Config { hub: Some(Default::default()), ..Default::default() });
            hub::serve_forever(&cfg, port, no_tailcat || cfg.tailcat.is_none())?
        }
        Cmd::Send { to, body, subject, reply_to, ttl, json } => {
            let ttl_secs = ttl.map(|t| parse_duration(&t)).transpose()?;
            let m = client(a)?.send(&NewMsg { to, body, subject, reply_to, ttl_secs, ..Default::default() })?;
            if json { print_json(&m)? } else { println!("sent #{} to {}", m.id, m.recipient) }
        }
        Cmd::Inbox { all, json } => {
            let v = client(a)?.inbox(all, 0.0)?;
            if json { print_json(&v)? } else { println!("{}", fmt::msgs(&v)) }
        }
        Cmd::Take { lease, json } => {
            let lease = lease.map(|l| parse_duration(&l)).transpose()?;
            if let Some(m) = client(a)?.take(lease)? {
                if json { print_json(&m)? } else { println!("{}", fmt::msg(&m)) }
            }
        }
        Cmd::Renew { id, lease } => {
            let m = client(a)?.renew(id, lease.map(|l| parse_duration(&l)).transpose()?)?;
            println!("#{} renewed; {} left", m.id, m.lease_until.map(fmt::until).unwrap_or_default());
        }
        Cmd::Done { id, note } => {
            let m = client(a)?.done(id, note.as_deref())?;
            let replied = note.is_some() && !m.no_reply && !m.broadcast;
            println!("#{} {}{}", m.id, if m.broadcast { "acknowledged" } else { "done" },
                     if replied { " (reply sent)" } else { "" });
        }
        Cmd::Release { id } => {
            client(a)?.release(id)?;
            println!("#{id} is back in the queue");
        }
        Cmd::Show { id, json } => {
            let v = client(a)?.thread(id)?;
            if json { print_json(&v)? } else { println!("{}", fmt::msgs(&v)) }
        }
        Cmd::Sent { json } => {
            let v = client(a)?.sent()?;
            if json { print_json(&v)? } else { println!("{}", fmt::msgs(&v)) }
        }
        Cmd::Dead { json } => {
            let v = client(a)?.dead()?;
            if json { print_json(&v)? } else { println!("{}", fmt::msgs(&v)) }
        }
        Cmd::Retry { id } => {
            client(a)?.retry(id)?;
            println!("#{id} is back in its queue");
        }
        Cmd::Watch { json, consumer } => {
            let c = client(a)?;
            loop {
                match c.watch(&consumer, 30.0) {
                    Ok((_, msgs)) => {
                        for m in &msgs {
                            if json { println!("{}", serde_json::to_string(m)?) } else { println!("{}", fmt::msg(m)) }
                        }
                        use std::io::Write;
                        std::io::stdout().flush()?;
                        if let Some(last) = msgs.iter().map(|m| m.id).max() {
                            let _ = c.ack(&consumer, last);
                        }
                    }
                    Err(e) => {
                        eprintln!("switchboard watch: {e:#}; retrying in 5s");
                        std::thread::sleep(std::time::Duration::from_secs(5));
                    }
                }
            }
        }
        Cmd::Lock { action, what, name, note, wait, ttl, force, fence } => {
            let c = client(a)?;
            let ttl = parse_duration(&ttl)?;
            match action.as_str() {
                "status" => println!("{}", fmt::lock(&c.lock_state(&name)?)),
                "list" => {
                    for l in c.locks()? {
                        println!("{}", fmt::lock(&l));
                    }
                }
                "take" => {
                    let note = note.or(what);
                    loop {
                        let (ok, st) = c.lock_take(&name, note.as_deref(), ttl)?;
                        if ok {
                            println!("lock {name}: yours, fence {}. Before acting: `switchboard lock check {}`. \
                                      Then `switchboard lock release`.", st.fence, st.fence);
                            break;
                        }
                        if !wait {
                            eprintln!("{}", fmt::lock(&st));
                            return Ok(2);
                        }
                        eprintln!("{} -- waiting", fmt::lock(&st));
                        std::thread::sleep(std::time::Duration::from_secs(5));
                    }
                }
                "renew" => println!("{}", fmt::lock(&c.lock_renew(&name, ttl)?)),
                "release" => {
                    c.lock_release(&name, note.or(what).as_deref(), force)?;
                    println!("lock {name}: released; watchers were told");
                }
                "check" => {
                    let f = fence.or_else(|| what.and_then(|w| w.parse().ok()));
                    let Some(f) = f else { bail!("lock check needs the fence: `switchboard lock check 7`") };
                    let (ok, st) = c.lock_check(&name, f)?;
                    if ok {
                        println!("VALID: fence {f} holds lock {name}");
                    } else {
                        eprintln!("INVALID: fence {f} does not hold lock {name}. {}", fmt::lock(&st));
                        return Ok(3);
                    }
                }
                other => bail!("unknown lock action {other} (status, take, renew, release, check, list)"),
            }
        }
        Cmd::Whoami => {
            let w = client(a)?.whoami()?;
            println!("{} (prefix {}{})", w["as"].as_str().unwrap_or(""), w["prefix"].as_str().unwrap_or(""),
                     if w["admin"].as_bool() == Some(true) { ", admin" } else { "" });
        }
        Cmd::Peers { remove } => {
            let c = client(a)?;
            if let Some(k) = remove {
                println!("{}", c.remove_peer(&k)?);
            } else {
                for p in c.peers()?.as_array().cloned().unwrap_or_default() {
                    println!("{}  {}  added {}", p["prefix"].as_str().unwrap_or(""), p["key"].as_str().unwrap_or(""),
                             fmt::when(p["added"].as_i64().unwrap_or(0)));
                }
            }
        }
        Cmd::Token { action, arg, admin } => {
            let c = client(a)?;
            match action.as_str() {
                "list" => print_json(&c.tokens()?)?,
                "create" => {
                    let Some(p) = arg else { bail!("token create PREFIX") };
                    print_json(&c.create_token(&p, "manual", admin)?)?
                }
                "revoke" => {
                    let Some(id) = arg.and_then(|s| s.parse().ok()) else { bail!("token revoke ID") };
                    print_json(&c.revoke_token(id)?)?
                }
                other => bail!("unknown token action {other}"),
            }
        }
        Cmd::ImportAgentMail { path } => {
            let cfg = Config::load()?;
            if cfg.role != "hub" {
                bail!("import runs on the hub");
            }
            let store = hub::open_store(&home(), &cfg.hub.unwrap_or_default())?;
            let (added, skipped) = store.import_agent_mail(&path)?;
            println!("imported {added} message(s) from {} ({skipped} already here)", path.display());
        }
        Cmd::Service { action } => match action.as_str() {
            "install" => {
                let exe = service::install_binary()?;
                println!("{}", service::install(&exe)?);
            }
            "uninstall" => {
                service::uninstall()?;
                println!("service removed");
            }
            "status" => {
                let (i, r, d) = service::status();
                println!("{d}: installed {i}, running {r}");
            }
            other => bail!("unknown service action {other} (install, uninstall, status)"),
        },
        Cmd::Agents { action } => match action.as_str() {
            "install" => {
                let exe = service::install_binary()?;
                for l in agents::install(&exe)? {
                    println!("{l}");
                }
            }
            "uninstall" => {
                for l in agents::uninstall()? {
                    println!("{l}");
                }
            }
            other => bail!("unknown agents action {other} (install, uninstall)"),
        },
        Cmd::Mcp => mcp::serve(client(a)?)?,
        Cmd::EnrollStdio => onboard::enroll_stdio()?,
    }
    Ok(0)
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("switchboard: {e:#}");
            std::process::exit(1);
        }
    }
}
