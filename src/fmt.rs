//! Human-readable lines for messages and locks.

use crate::store::{now_ms, LockState, Msg};

pub fn when(ms: i64) -> String {
    let ago = (now_ms() - ms) / 1000;
    match ago {
        i64::MIN..=59 => format!("{}s ago", ago.max(0)),
        60..=3599 => format!("{}m ago", ago / 60),
        3600..=86399 => format!("{}h ago", ago / 3600),
        _ => format!("{}d ago", ago / 86400),
    }
}

pub fn until(ms: i64) -> String {
    let left = (ms - now_ms()) / 1000;
    if left <= 0 {
        "now".into()
    } else if left < 120 {
        format!("{left}s")
    } else {
        format!("{}m", left / 60)
    }
}

pub fn msg(m: &Msg) -> String {
    let mut state = m.status.clone();
    if m.status == "leased" {
        state = format!("leased by {}, {} left", m.lease_owner.clone().unwrap_or_default(),
                        m.lease_until.map(until).unwrap_or_default());
    } else if m.status == "done" {
        if let Some(by) = &m.done_by {
            state = format!("done by {by}");
        }
    }
    if m.broadcast {
        state = match m.acked {
            Some(true) => "notice, acked".into(),
            _ => "notice".into(),
        };
    }
    let mut head = format!("#{} {} {} -> {} [{}]", m.id, when(m.ts), m.sender, m.recipient, state);
    if let Some(r) = m.reply_to {
        head.push_str(&format!(" re #{r}"));
    }
    if let Some(s) = &m.subject {
        head.push_str(&format!("  {s}"));
    }
    let mut text = format!("{head}\n    {}", m.body.replace('\n', "\n    "));
    if m.status == "done" {
        if let Some(n) = &m.note {
            text.push_str(&format!("\n    done: {n}"));
        }
    }
    text
}

pub fn msgs(v: &[Msg]) -> String {
    if v.is_empty() {
        "(none)".into()
    } else {
        v.iter().map(msg).collect::<Vec<_>>().join("\n")
    }
}

pub fn lock(st: &LockState) -> String {
    match &st.holder {
        None => format!("lock {}: free (fence {})", st.name, st.fence),
        Some(h) => {
            let mut s = format!("lock {}: held by {h} since {}, fence {}, expires in {}", st.name,
                                st.since.map(when).unwrap_or_default(), st.fence,
                                st.expires.map(until).unwrap_or_default());
            if let Some(n) = &st.note {
                s.push_str(&format!(" ({n})"));
            }
            if !st.waiters.is_empty() {
                s.push_str(&format!("; waiting: {}", st.waiters.join(", ")));
            }
            s
        }
    }
}
