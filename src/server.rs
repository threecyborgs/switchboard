//! The hub's HTTP API (loopback only; tailcat carries it to peers). Every call carries a bearer token and the mailbox
//! it acts as (`X-Switchboard-As`); the token's prefix decides which mailboxes it may act as.

use crate::config::{prefix_allows, valid_box, VERSION};
use crate::store::{now_ms, LockState, Msg, NewMsg, Refusal, Store};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

pub struct HubOptions {
    pub data_dir: PathBuf,
    pub min_free_mb: u64,
    pub lease_secs: u64,
    pub backups_keep: u32,
    /// Called after an invite is spent or a peer removed, so tailcat's allow list is rebuilt.
    pub on_peers_changed: Option<Box<dyn Fn() + Send + Sync>>,
    /// Called when an invite is created, so the enrollment listener starts.
    pub on_invite: Option<Box<dyn Fn() + Send + Sync>>,
    /// The tailcat address peers reach the hub at, once known.
    pub hub_addr: Option<Box<dyn Fn() -> Option<String> + Send + Sync>>,
    pub port: u16,
}

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub notify: Arc<Notify>,
    pub opts: Arc<HubOptions>,
    pub started: Instant,
}

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        match e.downcast_ref::<Refusal>() {
            Some(Refusal::NotFound(s)) => ApiError(StatusCode::NOT_FOUND, s.clone()),
            Some(Refusal::Conflict(s)) => ApiError(StatusCode::CONFLICT, s.clone()),
            Some(Refusal::Forbidden(s)) => ApiError(StatusCode::FORBIDDEN, s.clone()),
            Some(Refusal::Invalid(s)) => ApiError(StatusCode::BAD_REQUEST, s.clone()),
            None => {
                eprintln!("switchboard: internal error: {e:#}");
                ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
            }
        }
    }
}

type ApiResult<T> = Result<T, ApiError>;

fn err(code: StatusCode, msg: impl Into<String>) -> ApiError {
    ApiError(code, msg.into())
}

/// Who is calling: the mailbox it acts as, and what its token allows.
pub struct Caller {
    pub name: String,
    pub prefix: String,
    pub admin: bool,
}

fn caller(s: &AppState, h: &HeaderMap) -> ApiResult<Caller> {
    let token = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "missing bearer token (run `switchboard doctor`)"))?;
    let auth = s
        .store
        .auth(token)?
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "unknown or revoked token (run `switchboard doctor`)"))?;
    let name = match h.get("x-switchboard-as").and_then(|v| v.to_str().ok()).filter(|v| !v.is_empty()) {
        Some(n) => n.to_string(),
        None if auth.prefix == "*" => "admin".into(),
        None => auth.prefix.clone(),
    };
    if !valid_box(&name) {
        return Err(err(StatusCode::BAD_REQUEST, format!("bad mailbox name {name:?}")));
    }
    if !prefix_allows(&auth.prefix, &name) {
        return Err(err(StatusCode::FORBIDDEN,
                       format!("this token may act as {}/... only, not {name}", auth.prefix)));
    }
    Ok(Caller { name, prefix: auth.prefix, admin: auth.admin })
}

fn admin(s: &AppState, h: &HeaderMap) -> ApiResult<Caller> {
    let c = caller(s, h)?;
    if !c.admin {
        return Err(err(StatusCode::FORBIDDEN, "admin only (run it on the hub)"));
    }
    Ok(c)
}

impl AppState {
    fn changed(&self) {
        self.notify.notify_waiters();
    }

    /// Run `f` until it finds something or `wait` passes, waking on every change.
    async fn long_poll<T, F>(&self, wait: Duration, f: F) -> ApiResult<Vec<T>>
    where
        F: Fn() -> anyhow::Result<Vec<T>>,
    {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let found = f()?;
            if !found.is_empty() || tokio::time::Instant::now() >= deadline {
                return Ok(found);
            }
            let _ = tokio::time::timeout_at(deadline, notified).await;
        }
    }

    fn disk_ok(&self) -> ApiResult<()> {
        if self.opts.min_free_mb == 0 {
            return Ok(());
        }
        if let Ok(free) = fs2::available_space(&self.opts.data_dir) {
            if free < self.opts.min_free_mb * 1024 * 1024 {
                return Err(err(StatusCode::INSUFFICIENT_STORAGE, format!(
                    "the hub's disk has {} MB free (< {} MB): refusing new messages until space is freed",
                    free / 1024 / 1024, self.opts.min_free_mb)));
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct InboxQ {
    #[serde(default)]
    all: Option<bool>,
    #[serde(default)]
    after: Option<i64>,
    #[serde(default)]
    wait: Option<f64>,
}

#[derive(Deserialize, Default)]
struct LeaseB {
    #[serde(default)]
    lease_secs: Option<u64>,
}

#[derive(Deserialize, Default)]
struct DoneB {
    #[serde(default)]
    note: Option<String>,
}

#[derive(Deserialize)]
struct WatchQ {
    #[serde(default)]
    consumer: Option<String>,
    #[serde(default)]
    after: Option<i64>,
    #[serde(default)]
    wait: Option<f64>,
}

#[derive(Deserialize)]
struct AckB {
    #[serde(default)]
    consumer: Option<String>,
    last_id: i64,
}

#[derive(Deserialize, Default)]
struct LockB {
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    ttl_secs: Option<u64>,
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize)]
struct FenceQ {
    fence: i64,
}

fn wait_of(w: Option<f64>) -> Duration {
    Duration::from_secs_f64(w.unwrap_or(0.0).clamp(0.0, 60.0))
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/whoami", get(whoami))
        .route("/v1/messages", post(send))
        .route("/v1/messages/{id}", get(thread))
        .route("/v1/messages/{id}/renew", post(renew))
        .route("/v1/messages/{id}/done", post(done))
        .route("/v1/messages/{id}/release", post(release))
        .route("/v1/messages/{id}/retry", post(retry))
        .route("/v1/inbox", get(inbox))
        .route("/v1/take", post(take))
        .route("/v1/sent", get(sent))
        .route("/v1/dead", get(dead))
        .route("/v1/watch", get(watch))
        .route("/v1/watch/ack", post(ack))
        .route("/v1/locks", get(locks))
        .route("/v1/locks/{name}", get(lock_state))
        .route("/v1/locks/{name}/take", post(lock_take))
        .route("/v1/locks/{name}/renew", post(lock_renew))
        .route("/v1/locks/{name}/release", post(lock_release))
        .route("/v1/locks/{name}/check", get(lock_check))
        .route("/v1/admin/invites", post(invite))
        .route("/v1/admin/enroll", post(enroll))
        .route("/v1/admin/peers", get(peers))
        .route("/v1/admin/peers/remove", post(remove_peer))
        .route("/v1/admin/tokens", get(tokens).post(create_token))
        .route("/v1/admin/tokens/{id}/revoke", post(revoke_token))
        .with_state(state)
}

async fn healthz(State(s): State<AppState>, h: HeaderMap) -> ApiResult<Json<Value>> {
    let mut out = json!({"ok": true, "version": VERSION, "uptime_secs": s.started.elapsed().as_secs()});
    // Details only for a caller with a token.
    if caller(&s, &h).is_ok() {
        out["stats"] = s.store.stats()?;
        out["disk_free_mb"] = fs2::available_space(&s.opts.data_dir).map(|b| b / 1024 / 1024).unwrap_or(0).into();
        out["hub_addr_known"] = s.opts.hub_addr.as_ref().and_then(|f| f()).is_some().into();
    }
    Ok(Json(out))
}

async fn whoami(State(s): State<AppState>, h: HeaderMap) -> ApiResult<Json<Value>> {
    let c = caller(&s, &h)?;
    Ok(Json(json!({"as": c.name, "prefix": c.prefix, "admin": c.admin, "version": VERSION})))
}

async fn send(State(s): State<AppState>, h: HeaderMap, Json(b): Json<NewMsg>) -> ApiResult<Json<Msg>> {
    let c = caller(&s, &h)?;
    s.disk_ok()?;
    let mut b = b;
    if !c.admin {
        b.no_reply = false; // only the hub sends notices
    }
    let m = s.store.send(&c.name, &b)?;
    eprintln!("switchboard: #{} {} -> {}", m.id, m.sender, m.recipient);
    s.changed();
    Ok(Json(m))
}

async fn inbox(State(s): State<AppState>, h: HeaderMap, Query(q): Query<InboxQ>) -> ApiResult<Json<Vec<Msg>>> {
    let c = caller(&s, &h)?;
    let all = q.all.unwrap_or(false);
    let after = q.after.unwrap_or(0);
    let found = s.long_poll(wait_of(q.wait), || s.store.inbox(&c.name, all, after)).await?;
    Ok(Json(found))
}

async fn take(State(s): State<AppState>, h: HeaderMap, b: Option<Json<LeaseB>>) -> ApiResult<Response> {
    let c = caller(&s, &h)?;
    let lease = b.and_then(|b| b.lease_secs).unwrap_or(s.opts.lease_secs);
    match s.store.take(&c.name, lease)? {
        Some(m) => {
            s.changed();
            Ok(Json(m).into_response())
        }
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

async fn renew(State(s): State<AppState>, h: HeaderMap, Path(id): Path<i64>, b: Option<Json<LeaseB>>)
               -> ApiResult<Json<Msg>> {
    let c = caller(&s, &h)?;
    let lease = b.and_then(|b| b.lease_secs).unwrap_or(s.opts.lease_secs);
    Ok(Json(s.store.renew(&c.name, id, lease)?))
}

async fn done(State(s): State<AppState>, h: HeaderMap, Path(id): Path<i64>, b: Option<Json<DoneB>>)
              -> ApiResult<Json<Msg>> {
    let c = caller(&s, &h)?;
    let note = b.and_then(|b| b.0.note);
    let m = s.store.done(&c.name, &c.prefix, id, note.as_deref())?;
    s.changed();
    Ok(Json(m))
}

async fn release(State(s): State<AppState>, h: HeaderMap, Path(id): Path<i64>) -> ApiResult<Json<Msg>> {
    let c = caller(&s, &h)?;
    let m = s.store.release(&c.name, id)?;
    s.changed();
    Ok(Json(m))
}

async fn retry(State(s): State<AppState>, h: HeaderMap, Path(id): Path<i64>) -> ApiResult<Json<Msg>> {
    let c = caller(&s, &h)?;
    let m = s.store.retry(&c.prefix, id)?;
    s.changed();
    Ok(Json(m))
}

async fn thread(State(s): State<AppState>, h: HeaderMap, Path(id): Path<i64>) -> ApiResult<Json<Vec<Msg>>> {
    let c = caller(&s, &h)?;
    Ok(Json(s.store.thread(&c.prefix, id)?))
}

async fn sent(State(s): State<AppState>, h: HeaderMap) -> ApiResult<Json<Vec<Msg>>> {
    let c = caller(&s, &h)?;
    Ok(Json(s.store.sent(&c.name)?))
}

async fn dead(State(s): State<AppState>, h: HeaderMap) -> ApiResult<Json<Vec<Msg>>> {
    let c = caller(&s, &h)?;
    Ok(Json(s.store.dead(&c.prefix)?))
}

async fn watch(State(s): State<AppState>, h: HeaderMap, Query(q): Query<WatchQ>) -> ApiResult<Json<Value>> {
    let c = caller(&s, &h)?;
    let consumer = q.consumer.unwrap_or_else(|| "default".into());
    let cursor = std::sync::atomic::AtomicI64::new(0);
    let found = s
        .long_poll(wait_of(q.wait), || {
            let (cur, msgs) = s.store.watch(&c.name, &consumer, q.after)?;
            cursor.store(cur, std::sync::atomic::Ordering::Relaxed);
            Ok(msgs)
        })
        .await?;
    Ok(Json(json!({"cursor": cursor.load(std::sync::atomic::Ordering::Relaxed), "messages": found})))
}

async fn ack(State(s): State<AppState>, h: HeaderMap, Json(b): Json<AckB>) -> ApiResult<Json<Value>> {
    let c = caller(&s, &h)?;
    s.store.ack(&c.name, b.consumer.as_deref().unwrap_or("default"), b.last_id)?;
    Ok(Json(json!({"ok": true})))
}

async fn locks(State(s): State<AppState>, h: HeaderMap) -> ApiResult<Json<Vec<LockState>>> {
    caller(&s, &h)?;
    Ok(Json(s.store.locks()?))
}

async fn lock_state(State(s): State<AppState>, h: HeaderMap, Path(name): Path<String>) -> ApiResult<Json<LockState>> {
    caller(&s, &h)?;
    Ok(Json(s.store.lock_state(&name)?))
}

async fn lock_take(State(s): State<AppState>, h: HeaderMap, Path(name): Path<String>, b: Option<Json<LockB>>)
                   -> ApiResult<Response> {
    let c = caller(&s, &h)?;
    let b = b.map(|b| b.0).unwrap_or_default();
    let (ok, st) = s.store.lock_take(&name, &c.name, b.note.as_deref(), b.ttl_secs.unwrap_or(600))?;
    s.changed();
    eprintln!("switchboard: lock {name} {} {}", if ok { "taken by" } else { "refused to" }, c.name);
    Ok((if ok { StatusCode::OK } else { StatusCode::CONFLICT }, Json(st)).into_response())
}

async fn lock_renew(State(s): State<AppState>, h: HeaderMap, Path(name): Path<String>, b: Option<Json<LockB>>)
                    -> ApiResult<Json<LockState>> {
    let c = caller(&s, &h)?;
    let b = b.map(|b| b.0).unwrap_or_default();
    Ok(Json(s.store.lock_renew(&name, &c.name, b.ttl_secs.unwrap_or(600))?))
}

async fn lock_release(State(s): State<AppState>, h: HeaderMap, Path(name): Path<String>, b: Option<Json<LockB>>)
                      -> ApiResult<Json<LockState>> {
    let c = caller(&s, &h)?;
    let b = b.map(|b| b.0).unwrap_or_default();
    if b.force && !c.admin {
        return Err(err(StatusCode::FORBIDDEN, "--force is admin only: ask the holder, or run it on the hub"));
    }
    let st = s.store.lock_release(&name, &c.name, b.note.as_deref(), b.force)?;
    s.changed();
    eprintln!("switchboard: lock {name} released by {}", c.name);
    Ok(Json(st))
}

async fn lock_check(State(s): State<AppState>, h: HeaderMap, Path(name): Path<String>, Query(q): Query<FenceQ>)
                    -> ApiResult<Response> {
    caller(&s, &h)?;
    let (ok, st) = s.store.lock_check(&name, q.fence)?;
    Ok((if ok { StatusCode::OK } else { StatusCode::CONFLICT }, Json(json!({"valid": ok, "lock": st})))
        .into_response())
}

#[derive(Deserialize)]
struct InviteB {
    prefix: String,
    #[serde(default)]
    ttl_secs: Option<u64>,
}

async fn invite(State(s): State<AppState>, h: HeaderMap, Json(b): Json<InviteB>) -> ApiResult<Json<Value>> {
    let c = admin(&s, &h)?;
    let ttl = b.ttl_secs.unwrap_or(24 * 3600);
    let secret = s.store.create_invite(&b.prefix, &c.name, ttl)?;
    if let Some(f) = &s.opts.on_invite {
        f();
    }
    Ok(Json(json!({"secret": secret, "prefix": b.prefix, "expires": now_ms() + ttl as i64 * 1000})))
}

#[derive(Deserialize)]
struct EnrollB {
    secret: String,
    peer_key: String,
}

async fn enroll(State(s): State<AppState>, h: HeaderMap, Json(b): Json<EnrollB>) -> ApiResult<Json<Value>> {
    admin(&s, &h)?;
    let (prefix, token) = s.store.redeem_invite(&b.secret, &b.peer_key)?;
    eprintln!("switchboard: {prefix} enrolled from {}", b.peer_key);
    if let Some(f) = &s.opts.on_peers_changed {
        f();
    }
    s.changed();
    let addr = s.opts.hub_addr.as_ref().and_then(|f| f());
    Ok(Json(json!({"prefix": prefix, "token": token, "hub_addr": addr, "hub_port": s.opts.port})))
}

async fn peers(State(s): State<AppState>, h: HeaderMap) -> ApiResult<Json<Value>> {
    admin(&s, &h)?;
    Ok(Json(json!(s.store.peers()?)))
}

#[derive(Deserialize)]
struct KeyB {
    key: String,
}

async fn remove_peer(State(s): State<AppState>, h: HeaderMap, Json(b): Json<KeyB>) -> ApiResult<Json<Value>> {
    admin(&s, &h)?;
    let removed = s.store.remove_peer(&b.key)?;
    if let Some(f) = &s.opts.on_peers_changed {
        f();
    }
    Ok(Json(json!({"removed": removed})))
}

async fn tokens(State(s): State<AppState>, h: HeaderMap) -> ApiResult<Json<Value>> {
    admin(&s, &h)?;
    Ok(Json(json!(s.store.tokens()?)))
}

#[derive(Deserialize)]
struct TokenB {
    prefix: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    admin: bool,
}

async fn create_token(State(s): State<AppState>, h: HeaderMap, Json(b): Json<TokenB>) -> ApiResult<Json<Value>> {
    admin(&s, &h)?;
    let t = s.store.create_token(b.label.as_deref().unwrap_or("manual"), &b.prefix, b.admin)?;
    Ok(Json(json!({"token": t, "prefix": b.prefix, "admin": b.admin})))
}

async fn revoke_token(State(s): State<AppState>, h: HeaderMap, Path(id): Path<i64>) -> ApiResult<Json<Value>> {
    admin(&s, &h)?;
    Ok(Json(json!({"revoked": s.store.revoke_token(id)?})))
}

/// Daily backups as `backups/switchboard-YYYY-MM-DD.sqlite`, keeping the newest `keep`.
pub fn backup_if_due(store: &Store, dir: &std::path::Path, keep: u32) -> anyhow::Result<Option<PathBuf>> {
    if keep == 0 {
        return Ok(None);
    }
    let bdir = dir.join("backups");
    std::fs::create_dir_all(&bdir)?;
    let path = bdir.join(format!("switchboard-{}.sqlite", civil_date(now_ms())));
    if path.exists() {
        return Ok(None);
    }
    store.backup(&path)?;
    let mut all: Vec<_> = std::fs::read_dir(&bdir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).map(|n| n.starts_with("switchboard-")).unwrap_or(false))
        .collect();
    all.sort();
    while all.len() > keep as usize {
        let old = all.remove(0);
        let _ = std::fs::remove_file(old);
    }
    Ok(Some(path))
}

/// YYYY-MM-DD (UTC) from unix milliseconds.
pub fn civil_date(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Serve until the process ends: the API, the reaper (every 2 s) and the backup check (hourly).
pub async fn run(listener: tokio::net::TcpListener, state: AppState) -> anyhow::Result<()> {
    let reaper = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            match reaper.store.reap() {
                Ok(n) if n > 0 => reaper.changed(),
                Ok(_) => {}
                Err(e) => eprintln!("switchboard: reaper: {e:#}"),
            }
        }
    });
    let backups = state.clone();
    tokio::spawn(async move {
        loop {
            let st = backups.clone();
            let r = tokio::task::spawn_blocking(move || {
                backup_if_due(&st.store, &st.opts.data_dir, st.opts.backups_keep)
            }).await;
            match r {
                Ok(Ok(Some(p))) => eprintln!("switchboard: backup written to {}", p.display()),
                Ok(Err(e)) => eprintln!("switchboard: backup failed: {e:#}"),
                _ => {}
            }
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
    axum::serve(listener, router(state)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn dates() {
        assert_eq!(super::civil_date(0), "1970-01-01");
        assert_eq!(super::civil_date(1_790_812_800_000), "2026-10-01");
    }
}
