//! End-to-end tests against a real hub on a loopback port.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use switchboard::client::{Client, HubError};
use switchboard::config::HubConfig;
use switchboard::store::{NewMsg, Store};

struct Hub {
    url: String,
    admin: String,
    store: Arc<Store>,
    dir: std::path::PathBuf,
    _tmp: Option<tempfile::TempDir>,
}

impl Hub {
    fn client(&self, token: &str, as_: &str) -> Client {
        Client::new(&self.url, token, as_)
    }
    fn admin(&self, as_: &str) -> Client {
        self.client(&self.admin, as_)
    }
    fn token(&self, prefix: &str) -> String {
        self.store.create_token("test", prefix, false).unwrap()
    }
}

fn start_in(dir: std::path::PathBuf, tmp: Option<tempfile::TempDir>, hc: HubConfig) -> Hub {
    let st = switchboard::hub::state(&dir, &hc, None).unwrap();
    let store = st.store.clone();
    let admin = store.create_token("admin", "*", true).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(l.local_addr().unwrap().port()).unwrap();
            switchboard::server::run(l, st).await.unwrap();
        });
    });
    let port = rx.recv().unwrap();
    Hub { url: format!("http://127.0.0.1:{port}"), admin, store, dir, _tmp: tmp }
}

fn start(hc: HubConfig) -> Hub {
    let tmp = tempfile::tempdir().unwrap();
    start_in(tmp.path().to_path_buf(), Some(tmp), hc)
}

fn hc() -> HubConfig {
    HubConfig { min_free_mb: 0, backups_keep: 0, ..Default::default() }
}

fn status_of(e: anyhow::Error) -> u16 {
    e.downcast_ref::<HubError>().map(|h| h.status).unwrap_or(0)
}

fn send(c: &Client, to: &str, body: &str) -> i64 {
    c.send(&NewMsg { to: to.into(), body: body.into(), ..Default::default() }).unwrap().id
}

#[test]
fn tokens_are_scoped_to_their_prefix() {
    let h = start(hc());
    let bob = h.token("bob");
    let no = h.client("nonsense", "bob");
    assert_eq!(status_of(no.whoami().unwrap_err()), 401);
    let as_alice = h.client(&bob, "alice");
    assert_eq!(status_of(as_alice.send(&NewMsg { to: "alice".into(), body: "x".into(), ..Default::default() })
        .unwrap_err()), 403);
    let ok = h.client(&bob, "bob/merge");
    assert_eq!(ok.whoami().unwrap()["as"], "bob/merge");
    // Only admins make invites or force-release.
    assert_eq!(status_of(ok.invite("bob", 60).unwrap_err()), 403);
    assert_eq!(status_of(ok.lock_release("main", None, true).unwrap_err()), 403);
}

#[test]
fn take_done_and_reply() {
    let h = start(hc());
    let alice = h.admin("alice/coord");
    let bob = h.token("bob");
    let a = h.client(&bob, "bob/a");
    let b = h.client(&bob, "bob/b");
    let id = send(&alice, "bob", "check the installer on Windows");
    let m = a.take(None).unwrap().expect("a gets it");
    assert_eq!(m.id, id);
    assert!(m.lease_token.is_some());
    assert!(b.take(None).unwrap().is_none(), "b must not get a leased message");
    // b cannot close a's message
    assert_eq!(status_of(b.done(id, None).unwrap_err()), 409);
    a.done(id, Some("works, branch bob/installer-ok")).unwrap();
    // a notice from the hub is in the inbox but never handed out as work
    h.store.invite_notice_for_test("bob/a");
    assert!(a.take(None).unwrap().is_none(), "notices are not work");
    assert!(a.inbox(false, 0.0).unwrap().iter().any(|m| m.no_reply));
    let inbox = alice.inbox(false, 0.0).unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].reply_to, Some(id));
    assert_eq!(inbox[0].body, "works, branch bob/installer-ok");
    let thread = alice.thread(id).unwrap();
    assert_eq!(thread.len(), 2);
    // Done twice is harmless.
    a.done(id, None).unwrap();
}

#[test]
fn concurrent_takers_never_share_a_message() {
    let h = start(hc());
    let alice = h.admin("alice");
    for i in 0..200 {
        send(&alice, "work", &format!("job {i}"));
    }
    let tok = h.token("work");
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let c = h.client(&tok, &format!("work/w{t}"));
            let seen = seen.clone();
            std::thread::spawn(move || {
                while let Some(m) = c.take(None).unwrap() {
                    c.done(m.id, None).unwrap();
                    seen.lock().unwrap().push(m.id);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let mut v = seen.lock().unwrap().clone();
    assert_eq!(v.len(), 200);
    v.sort();
    v.dedup();
    assert_eq!(v.len(), 200, "a message was taken twice");
}

#[test]
fn lapsed_leases_requeue_then_dead_letter() {
    let h = start(HubConfig { max_attempts: 2, ..hc() });
    let alice = h.admin("alice");
    let w = h.client(&h.token("w"), "w");
    let id = send(&alice, "w", "crashy job");
    assert_eq!(w.take(Some(1)).unwrap().unwrap().id, id);
    std::thread::sleep(Duration::from_millis(1300));
    // the agent "crashed": the lease ran out, so the message is back
    let again = w.take(Some(1)).unwrap().expect("requeued after lease lapse");
    assert_eq!(again.id, id);
    assert_eq!(again.attempts, 2);
    // renewing keeps it
    w.renew(id, Some(1)).unwrap();
    std::thread::sleep(Duration::from_millis(1300));
    assert!(w.take(Some(1)).unwrap().is_none(), "two lapses with max_attempts=2 must dead-letter it");
    let dead = alice.dead().unwrap();
    assert_eq!(dead.iter().map(|m| m.id).collect::<Vec<_>>(), vec![id]);
    let notice = alice.inbox(false, 0.0).unwrap();
    assert!(notice.iter().any(|m| m.sender == "switchboard" && m.no_reply && m.body.contains("dead-letter")));
    alice.retry(id).unwrap();
    assert_eq!(w.take(None).unwrap().unwrap().id, id);
    // release hands it straight back
    w.release(id).unwrap();
    assert_eq!(w.take(None).unwrap().unwrap().id, id);
}

#[test]
fn idempotent_send_and_ttl() {
    let h = start(hc());
    let c = h.admin("alice");
    let m = NewMsg { to: "bob".into(), body: "once".into(), idempotency_key: Some("k1".into()), ..Default::default() };
    let a = c.send(&m).unwrap();
    let b = c.send(&m).unwrap();
    assert_eq!(a.id, b.id);
    let t = h.client(&h.token("bob"), "bob");
    assert_eq!(t.inbox(false, 0.0).unwrap().len(), 1);
    c.send(&NewMsg { to: "bob".into(), body: "stale".into(), ttl_secs: Some(1), ..Default::default() }).unwrap();
    assert_eq!(t.inbox(false, 0.0).unwrap().len(), 2);
    std::thread::sleep(Duration::from_millis(1200));
    let left = t.inbox(false, 0.0).unwrap();
    assert_eq!(left.len(), 1, "the ttl message expires unread");
}

#[test]
fn broadcasts_reach_everyone_and_ack_per_mailbox() {
    let h = start(hc());
    let alice = h.admin("alice");
    let tok = h.token("bob");
    let a = h.client(&tok, "bob/a");
    let b = h.client(&tok, "bob/b");
    let other = h.client(&h.token("carol"), "carol");
    let id = send(&alice, "bob/*", "freeze: no pushes for 10 min");
    assert_eq!(a.inbox(false, 0.0).unwrap()[0].id, id);
    assert_eq!(b.inbox(false, 0.0).unwrap()[0].id, id);
    assert!(other.inbox(false, 0.0).unwrap().is_empty(), "carol is not under bob/*");
    assert!(a.take(None).unwrap().is_none(), "broadcasts are notices, not work");
    a.done(id, Some("ack")).unwrap();
    assert!(a.inbox(false, 0.0).unwrap().is_empty());
    assert_eq!(b.inbox(false, 0.0).unwrap().len(), 1, "b has not acked yet");
    let all = send(&alice, "*", "hub restarting at 18:00");
    assert_eq!(other.inbox(false, 0.0).unwrap()[0].id, all);
}

#[test]
fn watch_cursor_survives_a_restart_and_long_poll_wakes_fast() {
    let tmp = tempfile::tempdir().unwrap();
    let h = start_in(tmp.path().to_path_buf(), None, hc());
    let alice = h.admin("alice");
    let tok = h.token("bob");
    let t = h.client(&tok, "bob");
    let (_, first) = t.watch("w", 0.0).unwrap();
    assert!(first.is_empty(), "a new consumer starts at the present");
    let id1 = send(&alice, "bob", "one");
    let (_, got) = t.watch("w", 0.0).unwrap();
    assert_eq!(got.iter().map(|m| m.id).collect::<Vec<_>>(), vec![id1]);
    // not acked yet: delivered again (at least once)
    assert_eq!(t.watch("w", 0.0).unwrap().1.len(), 1);
    t.ack("w", id1).unwrap();
    assert!(t.watch("w", 0.0).unwrap().1.is_empty());

    // long poll: a send from another thread wakes the waiter well before its 20 s timeout
    let s2 = alice.clone();
    let sender = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        send(&s2, "bob", "two")
    });
    let t0 = Instant::now();
    let (_, got) = t.watch("w", 20.0).unwrap();
    let id2 = sender.join().unwrap();
    assert_eq!(got[0].id, id2);
    assert!(t0.elapsed() < Duration::from_secs(3), "long poll took {:?}", t0.elapsed());

    // "restart": a fresh hub on the same database keeps messages and the cursor
    let h2 = start_in(tmp.path().to_path_buf(), None, hc());
    let t2 = h2.client(&tok, "bob");
    let (_, again) = t2.watch("w", 0.0).unwrap();
    assert_eq!(again.iter().map(|m| m.id).collect::<Vec<_>>(), vec![id2], "unacked id2 redelivered, id1 not");
    assert_eq!(t2.inbox(false, 0.0).unwrap().len(), 2);
    drop(h);
    drop(tmp);
}

#[test]
fn locks_expire_fence_and_notify() {
    let mut notify = BTreeMap::new();
    notify.insert("main".to_string(), vec!["alice".to_string(), "bob/merge".to_string()]);
    let h = start(HubConfig { lock_notify: notify, ..hc() });
    let alice_lane = h.admin("alice");
    let bob = h.client(&h.token("bob"), "bob/merge");
    let (ok, st) = alice_lane.lock_take("main", Some("feat/x"), 1).unwrap();
    assert!(ok);
    let fence1 = st.fence;
    assert!(alice_lane.lock_check("main", fence1).unwrap().0);
    let (ok, st) = bob.lock_take("main", Some("bob/y"), 60).unwrap();
    assert!(!ok);
    assert_eq!(st.waiters, vec!["bob/merge".to_string()]);
    // bob/merge was told when alice took it
    assert!(bob.inbox(false, 0.0).unwrap().iter().any(|m| m.subject.as_deref() == Some("lock main TAKEN by alice")));
    std::thread::sleep(Duration::from_millis(1300));
    // alice's lease lapsed: the waiter hears about it and gets a higher fence
    assert!(bob.inbox(false, 0.0).unwrap().iter().any(|m| m.subject.as_deref() == Some("lock main EXPIRED")));
    let (ok, st) = bob.lock_take("main", Some("bob/y"), 60).unwrap();
    assert!(ok);
    assert!(st.fence > fence1);
    // the stale holder's fence no longer passes: it must not push
    assert!(!alice_lane.lock_check("main", fence1).unwrap().0);
    assert!(bob.lock_check("main", st.fence).unwrap().0);
    assert_eq!(status_of(alice_lane.lock_release("main", None, false).unwrap_err()), 409);
    bob.lock_release("main", Some("pushed abc123"), false).unwrap();
    assert!(alice_lane.inbox(false, 0.0).unwrap().iter().any(|m| m.subject.as_deref() == Some("lock main FREE")
        && m.no_reply));
    // replying to a notice creates no message for the bot
    let n = alice_lane.inbox(false, 0.0).unwrap().into_iter().find(|m| m.sender == "switchboard").unwrap();
    alice_lane.done(n.id, Some("thanks")).unwrap();
    assert!(h.admin("switchboard").inbox(true, 0.0).unwrap().is_empty());
}

#[test]
fn disk_guard_refuses_sends() {
    let h = start(HubConfig { min_free_mb: u64::MAX / (1024 * 1024 * 2), ..hc() });
    let err = h.admin("alice").send(&NewMsg { to: "x".into(), body: "y".into(), ..Default::default() }).unwrap_err();
    assert_eq!(status_of(err), 507);
}

#[test]
fn invites_are_single_use() {
    let h = start(hc());
    let alice = h.admin("alice");
    let inv = alice.invite("bob", 3600).unwrap();
    let secret = inv["secret"].as_str().unwrap();
    let key = "nodekey:0123456789abcdef";
    let r = alice.enroll(secret, key).unwrap();
    assert_eq!(r["prefix"], "bob");
    let t = h.client(r["token"].as_str().unwrap(), "bob/docs");
    assert_eq!(t.whoami().unwrap()["prefix"], "bob");
    assert_eq!(status_of(alice.enroll(secret, key).unwrap_err()), 403);
    assert_eq!(alice.peers().unwrap()[0]["key"], key);
    // the inviter hears that bob joined
    assert!(alice.inbox(false, 0.0).unwrap().iter().any(|m| m.subject.as_deref() == Some("bob joined")));
    // a peer token can't enroll anyone
    assert_eq!(status_of(t.enroll("x", key).unwrap_err()), 403);
    let expired = alice.invite("bob", 0).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(status_of(alice.enroll(expired["secret"].as_str().unwrap(), key).unwrap_err()), 403);
}

#[test]
fn imports_the_python_agent_mail_database() {
    let tmp = tempfile::tempdir().unwrap();
    let old = tmp.path().join("agent-mail.sqlite");
    let c = rusqlite::Connection::open(&old).unwrap();
    c.execute_batch(
        "create table msg (id integer primary key, ts integer not null, sender text not null, recipient text not null,
           subject text, body text not null, reply_to integer, status text not null default 'open',
           taken_by text, taken_ts integer, done_ts integer, note text);
         insert into msg values (5, 1, 'alice', 'bob/merge', 's', 'open one', null, 'open', null, null, null, null);
         insert into msg values (6, 2, 'bob/merge', 'alice', null, 'stuck', null, 'taken', 'alice', 3, null, null);
         insert into msg values (7, 3, 'alice', 'bob', null, 'done one', null, 'done', 'bob/x', 4, 5, 'ok');
         insert into msg values (8, 4, 'bob/merge', 'merge-lock', null, 'thanks bot', 2, 'open', null, null, null, null);
         insert into msg values (9, 5, 'merge-lock', 'alice', 'lock FREE', 'free', null, 'open', null, null, null, null);",
    ).unwrap();
    drop(c);
    let h = start(hc());
    let (added, skipped) = h.store.import_agent_mail(&old).unwrap();
    assert_eq!((added, skipped), (5, 0));
    assert_eq!(h.store.import_agent_mail(&old).unwrap(), (0, 5));
    let alice = h.admin("alice");
    let ids: Vec<i64> = alice.inbox(false, 0.0).unwrap().iter().map(|m| m.id).collect();
    assert_eq!(ids, vec![6, 9], "the stuck 'taken' message is back in the queue; the bot reply is closed");
    let bot = alice.thread(9).unwrap();
    assert!(bot[0].no_reply);
    // new ids continue after the imported ones
    assert!(send(&alice, "bob", "new") > 9);
}

#[test]
fn backups_are_written_and_pruned() {
    let h = start(hc());
    send(&h.admin("alice"), "bob", "keep me");
    let p = switchboard::server::backup_if_due(&h.store, &h.dir, 2).unwrap().expect("first backup");
    assert!(p.exists());
    let copy = Store::open(&p, Default::default()).unwrap();
    assert_eq!(copy.inbox("bob", false, 0).unwrap().len(), 1);
    assert!(switchboard::server::backup_if_due(&h.store, &h.dir, 2).unwrap().is_none(), "once a day");
}

fn bin() -> String {
    env!("CARGO_BIN_EXE_switchboard").to_string()
}

#[test]
fn mcp_server_speaks_json_rpc() {
    let h = start(hc());
    let tok = h.token("bob");
    send(&h.admin("alice"), "bob/mcp", "please review PR 12");
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(bin())
        .arg("mcp")
        .env("SWITCHBOARD_HOME", home.path())
        .env("SWITCHBOARD_URL", &h.url)
        .env("SWITCHBOARD_TOKEN", &tok)
        .env("SWITCHBOARD_AS", "bob/mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut rpc = |id: i64, method: &str, params: Value| -> Value {
        writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        out.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };
    let init = rpc(1, "initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {},
                                           "clientInfo": {"name": "t", "version": "0"}}));
    assert_eq!(init["result"]["serverInfo"]["name"], "switchboard");
    let tools = rpc(2, "tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter()
        .map(|t| t["name"].as_str().unwrap()).collect();
    for want in ["send", "take", "done", "wait_for_mail", "lock_take", "lock_check"] {
        assert!(names.contains(&want), "missing tool {want}");
    }
    let took = rpc(3, "tools/call", json!({"name": "take", "arguments": {}}));
    let text = took["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("please review PR 12"), "{text}");
    let id: i64 = text.trim_start_matches('#').split_whitespace().next().unwrap().parse().unwrap();
    let done = rpc(4, "tools/call", json!({"name": "done", "arguments": {"id": id, "note": "LGTM"}}));
    assert!(done["result"]["content"][0]["text"].as_str().unwrap().contains("reply sent"));
    let bad = rpc(5, "tools/call", json!({"name": "send", "arguments": {"to": "alice"}}));
    assert_eq!(bad["result"]["isError"], true);
    let fenced = rpc(6, "tools/call", json!({"name": "lock_take", "arguments": {"note": "bob/x"}}));
    assert!(fenced["result"]["content"][0]["text"].as_str().unwrap().contains("fence 1"));
    drop(stdin);
    let _ = child.wait();
    let replies = h.admin("alice").inbox(false, 0.0).unwrap();
    assert!(replies.iter().any(|m| m.body == "LGTM" && m.sender == "bob/mcp"));
}

#[test]
fn cli_round_trip() {
    let h = start(hc());
    let tok = h.token("bob");
    let home = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| -> (i32, String) {
        let o = Command::new(bin())
            .args(args)
            .env("SWITCHBOARD_HOME", home.path())
            .env("SWITCHBOARD_URL", &h.url)
            .env("SWITCHBOARD_TOKEN", &tok)
            .env_remove("SWITCHBOARD_AS")
            .env_remove("AGENT_MAIL_AS")
            .output()
            .unwrap();
        (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).into_owned()
            + &String::from_utf8_lossy(&o.stderr))
    };
    let (code, out) = run(&["--as", "bob/a", "send", "bob/b", "hello b", "--subject", "hi"]);
    assert_eq!(code, 0, "{out}");
    let (_, out) = run(&["--as", "bob/b", "take"]);
    assert!(out.contains("hello b") && out.contains("leased by bob/b"), "{out}");
    let (_, out) = run(&["--as", "bob/b", "lock", "take", "feat/z"]);
    assert!(out.contains("fence 1"), "{out}");
    let (code, _) = run(&["--as", "bob/a", "lock", "take", "feat/q"]);
    assert_eq!(code, 2, "held lock exits 2");
    let (code, _) = run(&["--as", "bob/b", "lock", "check", "1"]);
    assert_eq!(code, 0);
    let (code, _) = run(&["--as", "bob/b", "lock", "check", "9"]);
    assert_eq!(code, 3, "stale fence exits 3");
    let (code, out) = run(&["--as", "alice", "inbox"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("bob/... only"), "{out}");
}
