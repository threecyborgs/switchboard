//! `switchboard mcp`: a Model Context Protocol server on stdio, so Claude Code, Codex and any MCP client get the
//! mailbox as native tools instead of shelling out.

use crate::client::Client;
use crate::fmt;
use crate::store::NewMsg;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::io::{BufRead, Write};

const PROTOCOL: &str = "2025-06-18";

fn tools() -> Value {
    let as_prop = json!({"type": "string", "description": "Mailbox to act as (person/topic, e.g. sean/netcode). Defaults to this machine's name or $SWITCHBOARD_AS."});
    let id = json!({"type": "integer", "description": "Message id"});
    let lock_name = json!({"type": "string", "description": "Lock name (default: main)"});
    let t = |name: &str, desc: &str, props: Value, required: &[&str]| {
        let mut p = props.as_object().cloned().unwrap_or_default();
        p.insert("as".into(), as_prop.clone());
        json!({"name": name, "description": desc,
               "inputSchema": {"type": "object", "properties": p, "required": required}})
    };
    json!([
        t("whoami", "Show which mailbox you act as, what your token allows, and whether the hub is reachable.", json!({}), &[]),
        t("send", "Send a message. `to` is a mailbox (tony/merge), a person (tony: any of their agents may take it), or a broadcast (tony/* or *).",
          json!({"to": {"type": "string"}, "body": {"type": "string"}, "subject": {"type": "string"},
                 "reply_to": {"type": "integer"}, "ttl_secs": {"type": "integer", "description": "expire unread after this many seconds"}}),
          &["to", "body"]),
        t("inbox", "List your open and leased messages plus unacknowledged notices (all=true adds finished ones).",
          json!({"all": {"type": "boolean"}}), &[]),
        t("take", "Claim the oldest open message under a lease. Finish it with `done`, extend with `renew`, or give it back with `release`. Returns nothing when the queue is empty.",
          json!({"lease_secs": {"type": "integer", "description": "default 900"}}), &[]),
        t("renew", "Extend your lease on a message you took.", json!({"id": id, "lease_secs": {"type": "integer"}}), &["id"]),
        t("done", "Finish a message (or acknowledge a notice). A note is sent back to the sender as a reply.",
          json!({"id": id, "note": {"type": "string"}}), &["id"]),
        t("release", "Give a message you took back to the queue for someone else.", json!({"id": id}), &["id"]),
        t("show", "Read a message and its whole reply thread.", json!({"id": id}), &["id"]),
        t("sent", "What you sent recently and its status.", json!({}), &[]),
        t("wait_for_mail", "Block until new mail arrives for you (or the timeout passes) and return it. Each message is returned once per consumer.",
          json!({"timeout_secs": {"type": "integer", "description": "max 55, default 50"},
                 "consumer": {"type": "string", "description": "cursor name, default mcp"}}), &[]),
        t("lock_status", "Who holds a lock, its fence, and who is waiting.", json!({"name": lock_name}), &[]),
        t("lock_take", "Take a lock for ttl_secs (default 600). Returns the fence number to pass to lock_check before acting. Waits up to wait_secs if it is held.",
          json!({"name": lock_name, "note": {"type": "string", "description": "what you hold it for, e.g. the branch"},
                 "ttl_secs": {"type": "integer"}, "wait_secs": {"type": "integer"}}), &[]),
        t("lock_renew", "Extend a lock you hold.", json!({"name": lock_name, "ttl_secs": {"type": "integer"}}), &[]),
        t("lock_check", "Check that your fence is still the live grant. Call right before the guarded action; if it fails, do not act.",
          json!({"name": lock_name, "fence": {"type": "integer"}}), &["fence"]),
        t("lock_release", "Release a lock you hold; waiters and watchers are told.", json!({"name": lock_name, "note": {"type": "string"}}), &[]),
    ])
}

fn s<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(|v| v.as_str())
}

fn i(a: &Value, k: &str) -> Option<i64> {
    a.get(k).and_then(|v| v.as_i64())
}

fn call_tool(base: &Client, name: &str, a: &Value) -> Result<String> {
    let mut c = base.clone();
    if let Some(as_) = s(a, "as") {
        c.as_ = as_.to_string();
    }
    let lock = s(a, "name").unwrap_or("main");
    Ok(match name {
        "whoami" => {
            let w = c.whoami()?;
            format!("You are {} (token prefix {}, admin {}). Hub {} is reachable.", w["as"].as_str().unwrap_or(""),
                    w["prefix"].as_str().unwrap_or(""), w["admin"], c.url)
        }
        "send" => {
            let m = c.send(&NewMsg {
                to: s(a, "to").ok_or_else(|| anyhow!("`to` is required"))?.into(),
                body: s(a, "body").ok_or_else(|| anyhow!("`body` is required"))?.into(),
                subject: s(a, "subject").map(String::from),
                reply_to: i(a, "reply_to"),
                ttl_secs: i(a, "ttl_secs").map(|t| t as u64),
                ..Default::default()
            })?;
            format!("sent #{} to {}", m.id, m.recipient)
        }
        "inbox" => fmt::msgs(&c.inbox(a.get("all").and_then(|v| v.as_bool()).unwrap_or(false), 0.0)?),
        "take" => match c.take(i(a, "lease_secs").map(|v| v as u64))? {
            Some(m) => format!("{}\n\nYou hold #{} until {}. `done` it with a note when finished.", fmt::msg(&m), m.id,
                               m.lease_until.map(fmt::until).map(|u| format!("{u} from now")).unwrap_or_default()),
            None => "Your queue is empty.".into(),
        },
        "renew" => {
            let m = c.renew(i(a, "id").ok_or_else(|| anyhow!("`id` is required"))?, i(a, "lease_secs").map(|v| v as u64))?;
            format!("#{} lease extended; {} left", m.id, m.lease_until.map(fmt::until).unwrap_or_default())
        }
        "done" => {
            let m = c.done(i(a, "id").ok_or_else(|| anyhow!("`id` is required"))?, s(a, "note"))?;
            format!("#{} {}{}", m.id, if m.broadcast { "acknowledged" } else { "done" },
                    if s(a, "note").is_some() && !m.no_reply && !m.broadcast { " (reply sent)" } else { "" })
        }
        "release" => {
            let m = c.release(i(a, "id").ok_or_else(|| anyhow!("`id` is required"))?)?;
            format!("#{} is back in the queue", m.id)
        }
        "show" => fmt::msgs(&c.thread(i(a, "id").ok_or_else(|| anyhow!("`id` is required"))?)?),
        "sent" => fmt::msgs(&c.sent()?),
        "wait_for_mail" => {
            let consumer = s(a, "consumer").unwrap_or("mcp").to_string();
            let wait = i(a, "timeout_secs").unwrap_or(50).clamp(1, 55) as f64;
            let (_, msgs) = c.watch(&consumer, wait)?;
            if let Some(last) = msgs.iter().map(|m| m.id).max() {
                c.ack(&consumer, last)?;
            }
            if msgs.is_empty() { "No new mail.".into() } else { fmt::msgs(&msgs) }
        }
        "lock_status" => fmt::lock(&c.lock_state(lock)?),
        "lock_take" => {
            let ttl = i(a, "ttl_secs").unwrap_or(600) as u64;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(i(a, "wait_secs").unwrap_or(0).clamp(0, 55) as u64);
            loop {
                let (ok, st) = c.lock_take(lock, s(a, "note"), ttl)?;
                if ok {
                    break format!("You hold lock {} with fence {}. Call lock_check with fence {} right before acting, \
                                   then lock_release.", st.name, st.fence, st.fence);
                }
                if std::time::Instant::now() >= deadline {
                    break format!("Not granted. {}. You are on the waiting list and will get a notice when it frees.",
                                  fmt::lock(&st));
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        }
        "lock_renew" => fmt::lock(&c.lock_renew(lock, i(a, "ttl_secs").unwrap_or(600) as u64)?),
        "lock_check" => {
            let (ok, st) = c.lock_check(lock, i(a, "fence").ok_or_else(|| anyhow!("`fence` is required"))?)?;
            if ok {
                format!("VALID: fence {} is the live grant on lock {}. Go ahead.", st.fence, st.name)
            } else {
                format!("INVALID: do not act. {}", fmt::lock(&st))
            }
        }
        "lock_release" => {
            c.lock_release(lock, s(a, "note"), false)?;
            format!("lock {lock} released")
        }
        other => return Err(anyhow!("unknown tool {other}")),
    })
}

pub fn serve(client: Client) -> Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let r = json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}});
                writeln!(out, "{r}")?;
                out.flush()?;
                continue;
            }
        };
        let Some(id) = req.get("id").cloned() else { continue }; // a notification
        let method = req["method"].as_str().unwrap_or("");
        let result: std::result::Result<Value, (i64, String)> = match method {
            "initialize" => {
                let asked = req["params"]["protocolVersion"].as_str().unwrap_or(PROTOCOL);
                Ok(json!({
                    "protocolVersion": asked,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "switchboard", "version": crate::config::VERSION},
                    "instructions": format!("switchboard: durable mail, work queue and locks shared with agents on \
                        other machines. You act as `{}` unless you pass `as` (use person/topic, e.g. {}/yourtask). \
                        take -> work -> done(note). Locks return a fence: lock_check it right before acting.",
                        client.as_, crate::store::person(&client.as_)),
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => {
                let name = req["params"]["name"].as_str().unwrap_or("");
                let args = req["params"].get("arguments").cloned().unwrap_or(json!({}));
                match call_tool(&client, name, &args) {
                    Ok(text) => Ok(json!({"content": [{"type": "text", "text": text}], "isError": false})),
                    Err(e) => Ok(json!({"content": [{"type": "text", "text": format!("error: {e:#}")}], "isError": true})),
                }
            }
            _ => Err((-32601, format!("method not found: {method}"))),
        };
        let resp = match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
        };
        writeln!(out, "{resp}")?;
        out.flush()?;
    }
    Ok(())
}
