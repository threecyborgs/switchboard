//! The hub's state, in one SQLite file (WAL). Every method runs under one mutex: the hub has a single writer, so a
//! `take` is atomic and nothing reads a half-written row.

use crate::config::{prefix_allows, valid_box, valid_destination};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use rand::RngCore;
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Mutex;

pub const SYSTEM: &str = "switchboard";

const SCHEMA: &str = r#"
create table if not exists meta (key text primary key, value text);
create table if not exists msg (
  id integer primary key autoincrement,
  ts integer not null, sender text not null, recipient text not null, broadcast integer not null default 0,
  subject text, body text not null, reply_to integer, no_reply integer not null default 0,
  idem_key text, expires_ts integer,
  status text not null default 'open',
  lease_owner text, lease_token text, lease_until integer, attempts integer not null default 0,
  done_by text, done_ts integer, note text);
create unique index if not exists msg_idem on msg(sender, idem_key) where idem_key is not null;
create index if not exists msg_to on msg(recipient, status, id);
create index if not exists msg_lease on msg(status, lease_until);
create index if not exists msg_reply on msg(reply_to);
create table if not exists acks (msg_id integer not null, box text not null, ts integer not null, note text,
  primary key (msg_id, box));
create table if not exists cursors (consumer text primary key, last_id integer not null);
create table if not exists locks (name text primary key, holder text, note text, fence integer not null default 0,
  since integer, expires integer);
create table if not exists lock_waiters (name text not null, who text not null, since integer not null,
  primary key (name, who));
create table if not exists tokens (id integer primary key, label text, prefix text not null,
  admin integer not null default 0, hash text not null unique, created integer not null, revoked integer);
create table if not exists invites (id integer primary key, prefix text not null, hash text not null unique,
  created_by text, created integer not null, expires integer not null, used_ts integer, peer_key text);
create table if not exists peers (key text primary key, prefix text not null, added integer not null);
"#;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Msg {
    pub id: i64,
    pub ts: i64,
    pub sender: String,
    pub recipient: String,
    pub broadcast: bool,
    pub subject: Option<String>,
    pub body: String,
    pub reply_to: Option<i64>,
    pub no_reply: bool,
    pub expires_ts: Option<i64>,
    /// open, leased, done, dead (lease ran out too often) or expired.
    pub status: String,
    pub lease_owner: Option<String>,
    /// Only returned to the caller that holds the lease.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_token: Option<String>,
    pub lease_until: Option<i64>,
    pub attempts: i64,
    pub done_by: Option<String>,
    pub done_ts: Option<i64>,
    pub note: Option<String>,
    /// For a broadcast: whether the asking mailbox has acknowledged it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acked: Option<bool>,
}

fn msg_from(r: &Row) -> rusqlite::Result<Msg> {
    Ok(Msg {
        id: r.get("id")?,
        ts: r.get("ts")?,
        sender: r.get("sender")?,
        recipient: r.get("recipient")?,
        broadcast: r.get::<_, i64>("broadcast")? != 0,
        subject: r.get("subject")?,
        body: r.get("body")?,
        reply_to: r.get("reply_to")?,
        no_reply: r.get::<_, i64>("no_reply")? != 0,
        expires_ts: r.get("expires_ts")?,
        status: r.get("status")?,
        lease_owner: r.get("lease_owner")?,
        lease_token: None,
        lease_until: r.get("lease_until")?,
        attempts: r.get("attempts")?,
        done_by: r.get("done_by")?,
        done_ts: r.get("done_ts")?,
        note: r.get("note")?,
        acked: None,
    })
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NewMsg {
    pub to: String,
    pub body: String,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub reply_to: Option<i64>,
    /// Retrying a send with the same key returns the first message instead of a duplicate.
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// The message expires unread after this long.
    #[serde(default)]
    pub ttl_secs: Option<u64>,
    /// A notice: `done --note` on it records the note but sends no reply.
    #[serde(default)]
    pub no_reply: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LockState {
    pub name: String,
    pub holder: Option<String>,
    pub note: Option<String>,
    /// Goes up by one every time the lock is granted. Pass it to `check` right before the guarded action: a holder
    /// whose lease ran out (and was replaced) gets refused.
    pub fence: i64,
    pub since: Option<i64>,
    pub expires: Option<i64>,
    pub waiters: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Auth {
    pub token_id: i64,
    pub label: Option<String>,
    pub prefix: String,
    pub admin: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    pub key: String,
    pub prefix: String,
    pub added: i64,
}

/// What a caller did wrong, kept apart from internal errors so the server can answer 4xx.
#[derive(Debug)]
pub enum Refusal {
    NotFound(String),
    Conflict(String),
    Forbidden(String),
    Invalid(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::NotFound(s) | Refusal::Conflict(s) | Refusal::Forbidden(s) | Refusal::Invalid(s) => f.write_str(s),
        }
    }
}
impl std::error::Error for Refusal {}

fn refuse<T>(r: Refusal) -> Result<T> {
    Err(anyhow!(r))
}

pub struct StoreOptions {
    pub max_attempts: u32,
    pub lock_notify: BTreeMap<String, Vec<String>>,
}

impl Default for StoreOptions {
    fn default() -> Self {
        StoreOptions { max_attempts: 5, lock_notify: BTreeMap::new() }
    }
}

pub struct Store {
    conn: Mutex<Connection>,
    opts: StoreOptions,
}

/// The person part of a mailbox: `tony/merge` -> `tony`.
pub fn person(name: &str) -> &str {
    name.split('/').next().unwrap_or(name)
}

pub fn random_secret(prefix: &str) -> String {
    let mut b = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut b);
    format!("{prefix}{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b))
}

pub fn hash_secret(s: &str) -> String {
    let d = Sha256::digest(s.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// SQL: this queue message is in `?1`'s inbox (sent to it, or to its person for any of them to take).
const QUEUE_FOR: &str = "(broadcast = 0 and (recipient = ?1 or recipient = ?2))";
/// SQL: this broadcast reaches `?1` (`*`, or `p/*` where ?1 is p or under p).
const BROADCAST_FOR: &str = "(broadcast = 1 and (recipient = '*' or substr(?1, 1, length(recipient) - 1) = \
     substr(recipient, 1, length(recipient) - 1) or ?1 = substr(recipient, 1, length(recipient) - 2)))";

impl Store {
    pub fn open(path: &Path, opts: StoreOptions) -> Result<Store> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Store::init(conn, opts)
    }

    pub fn open_memory() -> Result<Store> {
        Store::init(Connection::open_in_memory()?, StoreOptions::default())
    }

    fn init(conn: Connection, opts: StoreOptions) -> Result<Store> {
        conn.pragma_update(None, "journal_mode", "wal")?;
        conn.pragma_update(None, "synchronous", "normal")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch(SCHEMA)?;
        conn.execute("insert or ignore into meta (key, value) values ('schema', '1')", [])?;
        Ok(Store { conn: Mutex::new(conn), opts })
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        // A panic while holding the lock leaves SQLite consistent (every write is one statement or a transaction).
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    // ---- messages ------------------------------------------------------------------------------------------------

    pub fn send(&self, sender: &str, m: &NewMsg) -> Result<Msg> {
        let db = self.db();
        Self::send_in(&db, sender, m)
    }

    fn send_in(db: &Connection, sender: &str, m: &NewMsg) -> Result<Msg> {
        if !valid_destination(&m.to) {
            return refuse(Refusal::Invalid(format!("bad destination {:?} (a mailbox like tony/merge, tony/* or *)", m.to)));
        }
        if m.body.is_empty() {
            return refuse(Refusal::Invalid("body is empty".into()));
        }
        if m.body.len() > 256 * 1024 {
            return refuse(Refusal::Invalid("body over 256 KiB".into()));
        }
        if let Some(k) = &m.idempotency_key {
            if let Some(found) = db
                .query_row("select * from msg where sender = ?1 and idem_key = ?2", params![sender, k], msg_from)
                .optional()?
            {
                return Ok(found);
            }
        }
        if let Some(r) = m.reply_to {
            let exists: Option<i64> = db.query_row("select id from msg where id = ?1", [r], |r| r.get(0)).optional()?;
            if exists.is_none() {
                return refuse(Refusal::NotFound(format!("reply_to #{r} does not exist")));
            }
        }
        let broadcast = m.to.ends_with('*');
        let now = now_ms();
        let expires = m.ttl_secs.map(|t| now + (t as i64) * 1000);
        db.execute(
            "insert into msg (ts, sender, recipient, broadcast, subject, body, reply_to, no_reply, idem_key, expires_ts) \
             values (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![now, sender, m.to, broadcast as i64, m.subject, m.body, m.reply_to, m.no_reply as i64,
                    m.idempotency_key, expires],
        )?;
        let id = db.last_insert_rowid();
        Ok(db.query_row("select * from msg where id = ?1", [id], msg_from)?)
    }

    fn notice(db: &Connection, to: &str, subject: &str, body: &str) -> Result<()> {
        Self::send_in(db, SYSTEM, &NewMsg {
            to: to.into(),
            body: body.into(),
            subject: Some(subject.into()),
            no_reply: true,
            ..Default::default()
        })?;
        Ok(())
    }

    /// Inbox of `name`: `active` = open or leased queue messages plus unacknowledged broadcasts; `all` adds the rest.
    pub fn inbox(&self, name: &str, all: bool, after: i64) -> Result<Vec<Msg>> {
        let db = self.db();
        Self::reap_in(&db, &self.opts)?;
        let sql = if all {
            format!("select * from msg where ({QUEUE_FOR} or {BROADCAST_FOR}) and id > ?3 order by id")
        } else {
            format!(
                "select * from msg where (({QUEUE_FOR} and status in ('open','leased')) or ({BROADCAST_FOR} and \
                 status = 'open' and not exists (select 1 from acks where msg_id = msg.id and box = ?1))) \
                 and id > ?3 order by id"
            )
        };
        let mut st = db.prepare(&sql)?;
        let rows = st.query_map(params![name, person(name), after], msg_from)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Self::mark_acks(&db, name, &mut out)?;
        Ok(out)
    }

    fn mark_acks(db: &Connection, name: &str, msgs: &mut [Msg]) -> Result<()> {
        for m in msgs.iter_mut().filter(|m| m.broadcast) {
            let acked: Option<i64> = db
                .query_row("select 1 from acks where msg_id = ?1 and box = ?2", params![m.id, name], |r| r.get(0))
                .optional()?;
            m.acked = Some(acked.is_some());
        }
        Ok(())
    }

    /// Claim the oldest open queue message for `name` under a lease. Broadcasts are never taken.
    pub fn take(&self, name: &str, lease_secs: u64) -> Result<Option<Msg>> {
        let db = self.db();
        Self::reap_in(&db, &self.opts)?;
        let now = now_ms();
        let found: Option<i64> = db
            .query_row(
                // Notices (no_reply) are for reading, not work: `take` skips them.
                &format!("select id from msg where {QUEUE_FOR} and status = 'open' and no_reply = 0 order by id limit 1"),
                params![name, person(name)],
                |r| r.get(0),
            )
            .optional()?;
        let Some(id) = found else { return Ok(None) };
        let token = random_secret("lease_");
        db.execute(
            "update msg set status = 'leased', lease_owner = ?1, lease_token = ?2, lease_until = ?3, \
             attempts = attempts + 1 where id = ?4",
            params![name, token, now + lease_secs.max(1) as i64 * 1000, id],
        )?;
        let mut m = db.query_row("select * from msg where id = ?1", [id], msg_from)?;
        m.lease_token = Some(token);
        Ok(Some(m))
    }

    fn get(db: &Connection, id: i64) -> Result<Msg> {
        match db.query_row("select * from msg where id = ?1", [id], msg_from).optional()? {
            Some(m) => Ok(m),
            None => refuse(Refusal::NotFound(format!("no message #{id}"))),
        }
    }

    /// Whether `name` (acting under `prefix`) may see message `m`.
    fn visible(m: &Msg, prefix: &str) -> bool {
        if prefix == "*" || prefix_allows(prefix, &m.sender) {
            return true;
        }
        if m.broadcast {
            let p = m.recipient.trim_end_matches('*').trim_end_matches('/');
            return m.recipient == "*" || prefix_allows(p, prefix) || prefix_allows(prefix, p);
        }
        prefix_allows(prefix, &m.recipient) || (person(prefix) == m.recipient)
    }

    pub fn renew(&self, name: &str, id: i64, lease_secs: u64) -> Result<Msg> {
        let db = self.db();
        Self::reap_in(&db, &self.opts)?;
        let m = Self::get(&db, id)?;
        if m.status != "leased" || m.lease_owner.as_deref() != Some(name) {
            return refuse(Refusal::Conflict(format!(
                "#{id} is {} and not leased by {name}; take it again", m.status)));
        }
        db.execute("update msg set lease_until = ?1 where id = ?2",
                   params![now_ms() + lease_secs.max(1) as i64 * 1000, id])?;
        Self::get(&db, id)
    }

    /// Give a leased message back to the queue untouched.
    pub fn release(&self, name: &str, id: i64) -> Result<Msg> {
        let db = self.db();
        let m = Self::get(&db, id)?;
        if m.status != "leased" || m.lease_owner.as_deref() != Some(name) {
            return refuse(Refusal::Conflict(format!("#{id} is not leased by {name}")));
        }
        db.execute("update msg set status = 'open', lease_owner = null, lease_token = null, lease_until = null \
                    where id = ?1", [id])?;
        Self::get(&db, id)
    }

    /// Close a message. A note goes back to the sender as a reply, unless the message is a notice. On a broadcast this
    /// acknowledges it for `name` only.
    pub fn done(&self, name: &str, prefix: &str, id: i64, note: Option<&str>) -> Result<Msg> {
        let db = self.db();
        Self::reap_in(&db, &self.opts)?;
        let m = Self::get(&db, id)?;
        if !Self::visible(&m, prefix) {
            return refuse(Refusal::Forbidden(format!("#{id} is not addressed to {name}")));
        }
        let now = now_ms();
        if m.broadcast {
            db.execute("insert or replace into acks (msg_id, box, ts, note) values (?1,?2,?3,?4)",
                       params![id, name, now, note])?;
        } else {
            match m.status.as_str() {
                "done" | "dead" | "expired" => {
                    return Ok(m); // closing twice is fine; nothing changes
                }
                "leased" if m.lease_owner.as_deref() != Some(name) => {
                    return refuse(Refusal::Conflict(format!(
                        "#{id} is leased by {} until its lease runs out", m.lease_owner.unwrap_or_default())));
                }
                _ => {}
            }
            db.execute(
                "update msg set status = 'done', done_by = ?1, done_ts = ?2, note = ?3, lease_token = null \
                 where id = ?4",
                params![name, now, note, id],
            )?;
        }
        if let Some(n) = note.filter(|n| !n.is_empty()) {
            if !m.no_reply && m.sender != SYSTEM {
                let subject = format!("re: {}", m.subject.clone().unwrap_or_else(|| format!("#{id}")));
                Self::send_in(&db, name, &NewMsg {
                    to: m.sender.clone(),
                    body: n.into(),
                    subject: Some(subject),
                    reply_to: Some(id),
                    ..Default::default()
                })?;
            }
        }
        let mut out = vec![Self::get(&db, id)?];
        Self::mark_acks(&db, name, &mut out)?;
        Ok(out.remove(0))
    }

    /// A message and every reply around it, if `prefix` may see any of it.
    pub fn thread(&self, prefix: &str, id: i64) -> Result<Vec<Msg>> {
        let db = self.db();
        let mut root = Self::get(&db, id)?;
        while let Some(up) = root.reply_to {
            match db.query_row("select * from msg where id = ?1", [up], msg_from).optional()? {
                Some(p) => root = p,
                None => break,
            }
        }
        let mut out = vec![root.clone()];
        let mut frontier = vec![root.id];
        while !frontier.is_empty() {
            let mut next = Vec::new();
            for f in &frontier {
                let mut st = db.prepare("select * from msg where reply_to = ?1 order by id")?;
                for r in st.query_map([f], msg_from)? {
                    let r = r?;
                    next.push(r.id);
                    out.push(r);
                }
            }
            frontier = next;
        }
        out.sort_by_key(|m| m.id);
        if !out.iter().any(|m| Self::visible(m, prefix)) {
            return refuse(Refusal::Forbidden(format!("#{id} is not yours to read")));
        }
        Ok(out)
    }

    pub fn sent(&self, name: &str) -> Result<Vec<Msg>> {
        let db = self.db();
        let mut st = db.prepare("select * from msg where sender = ?1 order by id desc limit 50")?;
        let out = st.query_map([name], msg_from)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(out)
    }

    /// Messages whose lease ran out `max_attempts` times, for this prefix.
    pub fn dead(&self, prefix: &str) -> Result<Vec<Msg>> {
        let db = self.db();
        let mut st = db.prepare("select * from msg where status = 'dead' order by id desc limit 200")?;
        let out = st.query_map([], msg_from)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(out.into_iter().filter(|m| Self::visible(m, prefix)).collect())
    }

    /// Put a dead or expired message back in its queue.
    pub fn retry(&self, prefix: &str, id: i64) -> Result<Msg> {
        let db = self.db();
        let m = Self::get(&db, id)?;
        if !Self::visible(&m, prefix) {
            return refuse(Refusal::Forbidden(format!("#{id} is not yours")));
        }
        if !matches!(m.status.as_str(), "dead" | "expired") {
            return refuse(Refusal::Conflict(format!("#{id} is {}, not dead or expired", m.status)));
        }
        db.execute("update msg set status = 'open', attempts = 0, lease_owner = null, lease_until = null, \
                    expires_ts = null where id = ?1", [id])?;
        Self::get(&db, id)
    }

    // ---- watch cursors -------------------------------------------------------------------------------------------

    /// Everything new for `name` past the consumer's saved cursor (or `after`, when given). Nothing moves until `ack`.
    pub fn watch(&self, name: &str, consumer: &str, after: Option<i64>) -> Result<(i64, Vec<Msg>)> {
        let db = self.db();
        Self::reap_in(&db, &self.opts)?;
        let key = format!("{name}#{consumer}");
        let cursor = match after {
            Some(a) => a,
            None => match db
                .query_row("select last_id from cursors where consumer = ?1", [&key], |r| r.get::<_, i64>(0))
                .optional()?
            {
                Some(c) => c,
                None => {
                    // A new consumer starts at the present, not at the whole history.
                    let top: i64 = db.query_row("select coalesce(max(id), 0) from msg", [], |r| r.get(0))?;
                    db.execute("insert into cursors (consumer, last_id) values (?1, ?2)", params![key, top])?;
                    top
                }
            },
        };
        let mut st = db.prepare(&format!(
            "select * from msg where ({QUEUE_FOR} or {BROADCAST_FOR}) and id > ?3 order by id limit 100"))?;
        let mut out = st.query_map(params![name, person(name), cursor], msg_from)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Self::mark_acks(&db, name, &mut out)?;
        Ok((cursor, out))
    }

    pub fn ack(&self, name: &str, consumer: &str, last_id: i64) -> Result<()> {
        let db = self.db();
        db.execute(
            "insert into cursors (consumer, last_id) values (?1, ?2) on conflict(consumer) do update set \
             last_id = max(last_id, excluded.last_id)",
            params![format!("{name}#{consumer}"), last_id],
        )?;
        Ok(())
    }

    // ---- expiry --------------------------------------------------------------------------------------------------

    /// Return lapsed leases to their queue (or the dead-letter list), expire stale messages, free lapsed locks.
    /// Returns how many rows changed.
    pub fn reap(&self) -> Result<usize> {
        let db = self.db();
        Self::reap_in(&db, &self.opts)
    }

    fn reap_in(db: &Connection, opts: &StoreOptions) -> Result<usize> {
        let now = now_ms();
        let mut changed = 0;
        let lapsed: Vec<Msg> = {
            let mut st = db.prepare("select * from msg where status = 'leased' and lease_until < ?1")?;
            let v = st.query_map([now], msg_from)?.collect::<rusqlite::Result<Vec<_>>>()?;
            v
        };
        for m in lapsed {
            if m.attempts >= opts.max_attempts as i64 {
                db.execute("update msg set status = 'dead', lease_token = null where id = ?1", [m.id])?;
                if m.sender != SYSTEM {
                    Self::notice(db, &m.sender, &format!("dead letter #{}", m.id), &format!(
                        "#{} to {} was taken {} times and never finished (last by {}). It is on the dead-letter \
                         list: `switchboard dead`, `switchboard retry {}`.",
                        m.id, m.recipient, m.attempts, m.lease_owner.clone().unwrap_or_default(), m.id))?;
                }
            } else {
                db.execute("update msg set status = 'open', lease_token = null, lease_until = null where id = ?1",
                           [m.id])?;
            }
            changed += 1;
        }
        changed += db.execute(
            "update msg set status = 'expired' where status = 'open' and expires_ts is not null and expires_ts < ?1",
            [now],
        )?;
        let lapsed_locks: Vec<LockState> = {
            let mut st = db.prepare("select name from locks where holder is not null and expires < ?1")?;
            let names = st.query_map([now], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            names.iter().map(|n| Self::lock_state_in(db, n)).collect::<Result<Vec<_>>>()?
        };
        for l in lapsed_locks {
            db.execute("update locks set holder = null, note = null, since = null, expires = null where name = ?1",
                       [&l.name])?;
            let holder = l.holder.clone().unwrap_or_default();
            let mut to: BTreeSet<String> = opts.lock_notify.get(&l.name).cloned().unwrap_or_default().into_iter().collect();
            to.extend(l.waiters.iter().cloned());
            to.insert(holder.clone());
            for t in to {
                Self::notice(db, &t, &format!("lock {} EXPIRED", l.name), &format!(
                    "{holder}'s lease on lock {} (fence {}) ran out without a renew, so the lock is free. A holder \
                     that was still working must take it again; its old fence no longer passes `lock check`.",
                    l.name, l.fence))?;
            }
            changed += 1;
        }
        Ok(changed)
    }

    // ---- locks ---------------------------------------------------------------------------------------------------

    fn lock_state_in(db: &Connection, name: &str) -> Result<LockState> {
        let row = db
            .query_row("select holder, note, fence, since, expires from locks where name = ?1", [name], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .optional()?;
        let (holder, note, fence, since, expires) = row.unwrap_or((None, None, 0, None, None));
        let mut st = db.prepare("select who from lock_waiters where name = ?1 order by since")?;
        let waiters = st.query_map([name], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(LockState { name: name.into(), holder, note, fence, since, expires, waiters })
    }

    pub fn lock_state(&self, name: &str) -> Result<LockState> {
        let db = self.db();
        Self::reap_in(&db, &self.opts)?;
        Self::lock_state_in(&db, name)
    }

    pub fn locks(&self) -> Result<Vec<LockState>> {
        let db = self.db();
        Self::reap_in(&db, &self.opts)?;
        let mut st = db.prepare("select name from locks union select name from lock_waiters order by 1")?;
        let names = st.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        names.iter().map(|n| Self::lock_state_in(&db, n)).collect()
    }

    fn lock_tell(db: &Connection, opts: &StoreOptions, name: &str, actor: &str, extra: &[String], subject: &str,
                 body: &str) -> Result<()> {
        let mut to: BTreeSet<String> = opts.lock_notify.get(name).cloned().unwrap_or_default().into_iter().collect();
        to.extend(extra.iter().cloned());
        to.remove(actor);
        for t in to {
            Self::notice(db, &t, subject, body)?;
        }
        Ok(())
    }

    /// Take `name` for `ttl_secs`. Ok(state) when granted (holder == who); Err(Conflict) with the state otherwise,
    /// and `who` is recorded as waiting so it is told the moment the lock frees.
    pub fn lock_take(&self, name: &str, who: &str, note: Option<&str>, ttl_secs: u64) -> Result<(bool, LockState)> {
        if !valid_box(name) {
            return refuse(Refusal::Invalid(format!("bad lock name {name:?}")));
        }
        let db = self.db();
        Self::reap_in(&db, &self.opts)?;
        let st = Self::lock_state_in(&db, name)?;
        let now = now_ms();
        let expires = now + ttl_secs.max(1) as i64 * 1000;
        match st.holder.as_deref() {
            Some(h) if h == who => {
                // Taking it again while holding it just extends it, with the same fence.
                db.execute("update locks set expires = ?1, note = coalesce(?2, note) where name = ?3",
                           params![expires, note, name])?;
                return Ok((true, Self::lock_state_in(&db, name)?));
            }
            Some(_) => {
                db.execute("insert or ignore into lock_waiters (name, who, since) values (?1, ?2, ?3)",
                           params![name, who, now])?;
                return Ok((false, Self::lock_state_in(&db, name)?));
            }
            None => {}
        }
        db.execute(
            "insert into locks (name, holder, note, fence, since, expires) values (?1, ?2, ?3, 1, ?4, ?5) \
             on conflict(name) do update set holder = excluded.holder, note = excluded.note, fence = fence + 1, \
             since = excluded.since, expires = excluded.expires",
            params![name, who, note, now, expires],
        )?;
        db.execute("delete from lock_waiters where name = ?1 and who = ?2", params![name, who])?;
        let st = Self::lock_state_in(&db, name)?;
        Self::lock_tell(&db, &self.opts, name, who, &[], &format!("lock {name} TAKEN by {who}"), &format!(
            "{who} holds lock {name} (fence {}){}. Don't act on what it guards until it is released.",
            st.fence, note.map(|n| format!(" for {n}")).unwrap_or_default()))?;
        Ok((true, st))
    }

    pub fn lock_renew(&self, name: &str, who: &str, ttl_secs: u64) -> Result<LockState> {
        let db = self.db();
        Self::reap_in(&db, &self.opts)?;
        let st = Self::lock_state_in(&db, name)?;
        if st.holder.as_deref() != Some(who) {
            return refuse(Refusal::Conflict(format!("lock {name} is not held by {who}")));
        }
        db.execute("update locks set expires = ?1 where name = ?2",
                   params![now_ms() + ttl_secs.max(1) as i64 * 1000, name])?;
        Self::lock_state_in(&db, name)
    }

    pub fn lock_release(&self, name: &str, who: &str, note: Option<&str>, force: bool) -> Result<LockState> {
        let db = self.db();
        Self::reap_in(&db, &self.opts)?;
        let st = Self::lock_state_in(&db, name)?;
        let Some(holder) = st.holder.clone() else {
            db.execute("delete from lock_waiters where name = ?1 and who = ?2", params![name, who])?;
            return Self::lock_state_in(&db, name);
        };
        if holder != who && !force {
            return refuse(Refusal::Conflict(format!("lock {name} is held by {holder}, not {who}")));
        }
        db.execute("update locks set holder = null, note = null, since = null, expires = null where name = ?1",
                   [name])?;
        db.execute("delete from lock_waiters where name = ?1", [name])?;
        let how = if holder == who { "released".to_string() } else { format!("force-released by {who}") };
        let mut extra = st.waiters.clone();
        if holder != who {
            extra.push(holder.clone());
        }
        Self::lock_tell(&db, &self.opts, name, who, &extra, &format!("lock {name} FREE"), &format!(
            "Lock {name} is free: {holder} {how} it.{}", note.map(|n| format!("\n{n}")).unwrap_or_default()))?;
        Self::lock_state_in(&db, name)
    }

    /// True when `fence` is the live grant: the lock is held, unexpired, and nobody was granted it since.
    pub fn lock_check(&self, name: &str, fence: i64) -> Result<(bool, LockState)> {
        let db = self.db();
        Self::reap_in(&db, &self.opts)?;
        let st = Self::lock_state_in(&db, name)?;
        let ok = st.holder.is_some() && st.fence == fence && st.expires.map(|e| e > now_ms()).unwrap_or(false);
        Ok((ok, st))
    }

    // ---- tokens, invites, peers ----------------------------------------------------------------------------------

    pub fn create_token(&self, label: &str, prefix: &str, admin: bool) -> Result<String> {
        if prefix != "*" && !valid_box(prefix) {
            bail!("bad prefix {prefix:?}");
        }
        let token = random_secret("sb_");
        self.db().execute(
            "insert into tokens (label, prefix, admin, hash, created) values (?1, ?2, ?3, ?4, ?5)",
            params![label, prefix, admin as i64, hash_secret(&token), now_ms()],
        )?;
        Ok(token)
    }

    pub fn auth(&self, token: &str) -> Result<Option<Auth>> {
        let db = self.db();
        Ok(db
            .query_row(
                "select id, label, prefix, admin from tokens where hash = ?1 and revoked is null",
                [hash_secret(token)],
                |r| Ok(Auth { token_id: r.get(0)?, label: r.get(1)?, prefix: r.get(2)?, admin: r.get::<_, i64>(3)? != 0 }),
            )
            .optional()?)
    }

    pub fn tokens(&self) -> Result<Vec<serde_json::Value>> {
        let db = self.db();
        let mut st = db.prepare("select id, label, prefix, admin, created, revoked from tokens order by id")?;
        let v = st.query_map([], |r| {
            Ok(serde_json::json!({"id": r.get::<_, i64>(0)?, "label": r.get::<_, Option<String>>(1)?,
                "prefix": r.get::<_, String>(2)?, "admin": r.get::<_, i64>(3)? != 0,
                "created": r.get::<_, i64>(4)?, "revoked": r.get::<_, Option<i64>>(5)?}))
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    pub fn revoke_token(&self, id: i64) -> Result<bool> {
        Ok(self.db().execute("update tokens set revoked = ?1 where id = ?2 and revoked is null",
                             params![now_ms(), id])? > 0)
    }

    pub fn create_invite(&self, prefix: &str, created_by: &str, ttl_secs: u64) -> Result<String> {
        if !valid_box(prefix) || prefix.contains('/') {
            return refuse(Refusal::Invalid(format!("invite a person name like `tony`, not {prefix:?}")));
        }
        let secret = random_secret("inv_");
        let now = now_ms();
        self.db().execute(
            "insert into invites (prefix, hash, created_by, created, expires) values (?1, ?2, ?3, ?4, ?5)",
            params![prefix, hash_secret(&secret), created_by, now, now + ttl_secs as i64 * 1000],
        )?;
        Ok(secret)
    }

    pub fn pending_invites(&self) -> Result<i64> {
        Ok(self.db().query_row("select count(*) from invites where used_ts is null and expires > ?1", [now_ms()],
                               |r| r.get(0))?)
    }

    /// Spend an invite: allow the peer's tailcat key and mint it a token for the invite's prefix.
    pub fn redeem_invite(&self, secret: &str, peer_key: &str) -> Result<(String, String)> {
        let db = self.db();
        let now = now_ms();
        let row: Option<(i64, String, Option<String>, i64, Option<i64>)> = db
            .query_row("select id, prefix, created_by, expires, used_ts from invites where hash = ?1",
                       [hash_secret(secret)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .optional()?;
        let Some((id, prefix, created_by, expires, used)) = row else {
            return refuse(Refusal::Forbidden("unknown invite".into()));
        };
        if used.is_some() {
            return refuse(Refusal::Forbidden("this invite was already used; ask for a new one".into()));
        }
        if expires < now {
            return refuse(Refusal::Forbidden("this invite expired; ask for a new one".into()));
        }
        if !peer_key.starts_with("nodekey:") {
            return refuse(Refusal::Invalid(format!("bad peer key {peer_key:?}")));
        }
        db.execute("update invites set used_ts = ?1, peer_key = ?2 where id = ?3", params![now, peer_key, id])?;
        db.execute("insert or replace into peers (key, prefix, added) values (?1, ?2, ?3)",
                   params![peer_key, prefix, now])?;
        drop(db);
        let token = self.create_token(&format!("peer {prefix} {}", &peer_key[..peer_key.len().min(20)]), &prefix,
                                      false)?;
        if let Some(by) = created_by.filter(|b| valid_box(b)) {
            self.send(SYSTEM, &NewMsg {
                to: by,
                subject: Some(format!("{prefix} joined")),
                body: format!("{prefix} joined the switchboard from tailcat key {peer_key}. Say hi: \
                               `switchboard send {prefix} \"hello\"`."),
                no_reply: true,
                ..Default::default()
            })?;
        }
        Ok((prefix, token))
    }

    #[doc(hidden)]
    pub fn invite_notice_for_test(&self, to: &str) {
        let db = self.db();
        Self::notice(&db, to, "test notice", "for reading only").unwrap();
    }

    pub fn peers(&self) -> Result<Vec<Peer>> {
        let db = self.db();
        let mut st = db.prepare("select key, prefix, added from peers order by added")?;
        let v = st.query_map([], |r| Ok(Peer { key: r.get(0)?, prefix: r.get(1)?, added: r.get(2)? }))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    pub fn add_peer(&self, key: &str, prefix: &str) -> Result<()> {
        self.db().execute("insert or replace into peers (key, prefix, added) values (?1, ?2, ?3)",
                          params![key, prefix, now_ms()])?;
        Ok(())
    }

    pub fn remove_peer(&self, key: &str) -> Result<bool> {
        Ok(self.db().execute("delete from peers where key = ?1", [key])? > 0)
    }

    // ---- upkeep --------------------------------------------------------------------------------------------------

    pub fn stats(&self) -> Result<serde_json::Value> {
        let db = self.db();
        let mut st = db.prepare("select status, count(*) from msg group by status")?;
        let mut by = serde_json::Map::new();
        for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
            let (s, n) = r?;
            by.insert(s, n.into());
        }
        let peers: i64 = db.query_row("select count(*) from peers", [], |r| r.get(0))?;
        Ok(serde_json::json!({"messages": by, "peers": peers}))
    }

    /// A consistent copy of the database, safe while the hub runs.
    pub fn backup(&self, to: &Path) -> Result<()> {
        let _ = std::fs::remove_file(to);
        self.db().execute("vacuum into ?1", [to.to_string_lossy()])?;
        Ok(())
    }

    /// Bring in the Python agent-mail hub's database, keeping message ids. Safe to run twice.
    pub fn import_agent_mail(&self, path: &Path) -> Result<(usize, usize)> {
        let old = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut st = old.prepare("select id, ts, sender, recipient, subject, body, reply_to, status, taken_by, \
                                  done_ts, note from msg order by id")?;
        let rows = st.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?, r.get::<_, String>(5)?, r.get::<_, Option<i64>>(6)?,
                r.get::<_, String>(7)?, r.get::<_, Option<String>>(8)?, r.get::<_, Option<i64>>(9)?,
                r.get::<_, Option<String>>(10)?))
        })?;
        let db = self.db();
        let (mut added, mut skipped) = (0, 0);
        for r in rows {
            let (id, ts, sender, recipient, subject, body, reply_to, status, taken_by, done_ts, note) = r?;
            // The old hub's lock bot sent as `merge-lock`; replies to it piled up in a mailbox nobody read.
            let bot_reply = recipient == "merge-lock";
            let no_reply = sender == "merge-lock";
            let (status, note) = match (status.as_str(), bot_reply) {
                (_, true) => ("done".to_string(), Some("imported: reply to the old lock bot, closed".to_string())),
                ("done", _) => ("done".to_string(), note),
                _ => ("open".to_string(), note), // 'taken' had no lease; it goes back in the queue
            };
            let n = db.execute(
                "insert or ignore into msg (id, ts, sender, recipient, broadcast, subject, body, reply_to, no_reply, \
                 status, done_by, done_ts, note) values (?1,?2,?3,?4,0,?5,?6,?7,?8,?9,?10,?11,?12)",
                params![id, ts, sender, recipient, subject, body, reply_to, no_reply as i64, status,
                        if status == "done" { taken_by } else { None }, done_ts, note],
            )?;
            if n > 0 { added += 1 } else { skipped += 1 }
        }
        Ok((added, skipped))
    }
}
