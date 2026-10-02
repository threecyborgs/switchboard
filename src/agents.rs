//! Hook switchboard into the coding agents on this machine: an MCP server entry for Claude Code and Codex, and a
//! short skill telling an agent how to work the queue.

use crate::config::{home, is_default_home, user_home};
use anyhow::Result;
use std::path::Path;
use std::process::Command;

pub const SKILL: &str = r#"---
name: switchboard
description: Message, hand work to, and coordinate with agents on other machines (and other people's agents) through the switchboard hub. Use when you need to send someone's agent a task or question, work your own queue, wait for a reply, or hold a shared lock (like "only one of us pushes main at a time").
---

# switchboard

A durable mailbox, work queue and lock service shared by every agent on every enrolled machine. Messages live on the
hub until someone finishes them, so nothing is lost when an agent or a machine goes away.

Use the `switchboard` MCP tools when you have them; otherwise the `switchboard` command does the same things.

## Who you are

You act as a mailbox named `person/topic`, like `sean/netcode` or `tony/merge`. Pick one that says what you work on
and pass it as `as` (MCP) or `--as` / `SWITCHBOARD_AS` (CLI). A message sent to `tony` can be taken by any `tony/...`
agent; one sent to `tony/merge` only by that one. `tony/*` or `*` broadcasts a notice to everyone under it.

## Working the queue

1. `take` the oldest open message. You get a lease (15 minutes by default).
2. Do the work. If it runs long, `renew` the lease; if you can't do it, `release` it so someone else can.
3. `done` it with a `note`. The note goes back to the sender as a reply.

A lease that runs out puts the message back in the queue, and after 5 lapses it goes to the dead-letter list. So a
crash never loses work, but an agent that takes and forgets will be noticed.

To wait for new mail, call `wait_for_mail` (MCP) or run `switchboard watch` under a Monitor.

## Locks

`lock_take` returns a `fence` number. Right before the guarded action (for example `git push` to main), call
`lock_check` with that fence: if your lease ran out and someone else took the lock, the check fails and you must not
act. Release the lock as soon as you are done. Locks expire (10 minutes by default) unless renewed.

## Etiquette

- Say what you want, what you already know, and what "done" looks like. One task per message.
- Reply with `done` + note, not a new message, so the thread stays together (`show ID` reads a thread).
- Notices from `switchboard` (lock taken/free, someone joined) need no reply.
"#;

fn which(name: &str) -> Option<String> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).map(|d| d.join(name)).find(|p| p.is_file()).map(|p| p.to_string_lossy().into())
}

fn env_json() -> serde_json::Value {
    if is_default_home() {
        serde_json::json!({})
    } else {
        serde_json::json!({"SWITCHBOARD_HOME": home().to_string_lossy()})
    }
}

/// Register the MCP server and skill with every agent found here. Returns one line per thing done.
pub fn install(exe: &Path) -> Result<Vec<String>> {
    let mut done = Vec::new();
    let exe_s = exe.to_string_lossy().to_string();

    // Claude Code
    if let Some(claude) = which("claude") {
        let _ = Command::new(&claude).args(["mcp", "remove", "-s", "user", "switchboard"]).output();
        let spec = serde_json::json!({"type": "stdio", "command": exe_s, "args": ["mcp"], "env": env_json()});
        let out = Command::new(&claude)
            .args(["mcp", "add-json", "-s", "user", "switchboard", &spec.to_string()])
            .output();
        match out {
            Ok(o) if o.status.success() => done.push("Claude Code: MCP server `switchboard` added (user scope)".into()),
            Ok(o) => done.push(format!("Claude Code: could not add the MCP server ({})",
                                       String::from_utf8_lossy(&o.stderr).trim())),
            Err(e) => done.push(format!("Claude Code: could not run claude ({e})")),
        }
    }
    let claude_dir = user_home().join(".claude");
    if claude_dir.is_dir() {
        let d = claude_dir.join("skills/switchboard");
        std::fs::create_dir_all(&d)?;
        std::fs::write(d.join("SKILL.md"), SKILL)?;
        done.push(format!("Claude Code: skill written to {}", d.join("SKILL.md").display()));
    }

    // Codex
    let codex_dir = user_home().join(".codex");
    if codex_dir.is_dir() || which("codex").is_some() {
        std::fs::create_dir_all(&codex_dir)?;
        let cfg = codex_dir.join("config.toml");
        let text = std::fs::read_to_string(&cfg).unwrap_or_default();
        let mut block = format!("[mcp_servers.switchboard]\ncommand = {:?}\nargs = [\"mcp\"]\n", exe_s);
        if !is_default_home() {
            block.push_str(&format!("\n[mcp_servers.switchboard.env]\nSWITCHBOARD_HOME = {:?}\n",
                                    home().to_string_lossy()));
        }
        let new = replace_toml_table(&text, "mcp_servers.switchboard", &block);
        std::fs::write(&cfg, new)?;
        done.push(format!("Codex: MCP server `switchboard` added to {}", cfg.display()));
        let d = codex_dir.join("skills/switchboard");
        std::fs::create_dir_all(&d)?;
        std::fs::write(d.join("SKILL.md"), SKILL)?;
        done.push(format!("Codex: skill written to {}", d.join("SKILL.md").display()));
    }
    if done.is_empty() {
        done.push("no Claude Code or Codex found; agents can still run the `switchboard` command".into());
    }
    Ok(done)
}

pub fn uninstall() -> Result<Vec<String>> {
    let mut done = Vec::new();
    if let Some(claude) = which("claude") {
        let _ = Command::new(&claude).args(["mcp", "remove", "-s", "user", "switchboard"]).output();
        done.push("Claude Code: MCP server removed".to_string());
    }
    let _ = std::fs::remove_file(user_home().join(".claude/skills/switchboard/SKILL.md"));
    let _ = std::fs::remove_dir(user_home().join(".claude/skills/switchboard"));
    let cfg = user_home().join(".codex/config.toml");
    if let Ok(text) = std::fs::read_to_string(&cfg) {
        std::fs::write(&cfg, replace_toml_table(&text, "mcp_servers.switchboard", ""))?;
        done.push("Codex: MCP server removed".to_string());
    }
    let _ = std::fs::remove_file(user_home().join(".codex/skills/switchboard/SKILL.md"));
    let _ = std::fs::remove_dir(user_home().join(".codex/skills/switchboard"));
    Ok(done)
}

/// (claude registered, codex registered)
pub fn status() -> (Option<bool>, Option<bool>) {
    let claude = which("claude").map(|c| {
        Command::new(c).args(["mcp", "get", "switchboard"]).output().map(|o| o.status.success()).unwrap_or(false)
    });
    let cfg = user_home().join(".codex/config.toml");
    let codex = std::fs::read_to_string(&cfg).ok().map(|t| t.contains("[mcp_servers.switchboard]"));
    (claude, codex)
}

/// Replace `[table]` and its `[table.sub]` tables with `block` (appending when absent).
pub fn replace_toml_table(text: &str, table: &str, block: &str) -> String {
    let mut out = Vec::new();
    let mut skipping = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            let name = t.trim_start_matches('[').trim_end_matches(']').trim();
            skipping = name == table || name.starts_with(&format!("{table}."));
        }
        if !skipping {
            out.push(line);
        }
    }
    let mut s = out.join("\n");
    while s.ends_with("\n\n") {
        s.pop();
    }
    if !block.is_empty() {
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        if !s.is_empty() {
            s.push('\n');
        }
        s.push_str(block);
    } else if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    #[test]
    fn toml_table() {
        let t = "a = 1\n\n[mcp_servers.x]\ncommand = \"x\"\n\n[mcp_servers.switchboard]\ncommand = \"old\"\n\
                 [mcp_servers.switchboard.env]\nA = \"b\"\n[other]\nk = 2\n";
        let r = super::replace_toml_table(t, "mcp_servers.switchboard", "[mcp_servers.switchboard]\ncommand = \"new\"\n");
        assert!(r.contains("command = \"new\""));
        assert!(!r.contains("old"));
        assert!(!r.contains("A = \"b\""));
        assert!(r.contains("[other]\nk = 2"));
        assert!(r.contains("[mcp_servers.x]"));
    }
}
