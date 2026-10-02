//! A blocking client for the hub's API, used by the CLI and the MCP server.

use crate::store::{LockState, Msg, NewMsg};
use anyhow::{anyhow, bail, Result};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Clone)]
pub struct Client {
    pub url: String,
    pub token: String,
    pub as_: String,
    agent: ureq::Agent,
}

/// An error the hub answered with (as opposed to not answering at all).
#[derive(Debug)]
pub struct HubError {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for HubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "hub said {}: {}", self.status, self.message)
    }
}
impl std::error::Error for HubError {}

pub enum Reply {
    Ok(Value),
    NoContent,
    /// 409 with a body (a held lock, a stale fence).
    Conflict(Value),
}

impl Client {
    pub fn new(url: &str, token: &str, as_: &str) -> Client {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout_read(Duration::from_secs(75))
            .build();
        Client { url: url.trim_end_matches('/').into(), token: token.into(), as_: as_.into(), agent }
    }

    fn raw(&self, method: &str, path: &str, body: Option<&Value>, timeout: Duration) -> Result<Reply> {
        let mut req = self
            .agent
            .request(method, &format!("{}{}", self.url, path))
            .timeout(timeout)
            .set("authorization", &format!("Bearer {}", self.token));
        if !self.as_.is_empty() {
            req = req.set("x-switchboard-as", &self.as_);
        }
        let res = match body {
            Some(b) => req.send_json(b.clone()),
            None if method == "POST" => req.send_json(json!({})),
            None => req.call(),
        };
        match res {
            Ok(r) if r.status() == 204 => Ok(Reply::NoContent),
            Ok(r) => Ok(Reply::Ok(r.into_json()?)),
            Err(ureq::Error::Status(409, r)) => Ok(Reply::Conflict(r.into_json().unwrap_or(Value::Null))),
            Err(ureq::Error::Status(code, r)) => {
                let v: Value = r.into_json().unwrap_or(Value::Null);
                let message = v.get("error").and_then(|e| e.as_str()).unwrap_or("").to_string();
                Err(anyhow!(HubError { status: code, message }))
            }
            Err(ureq::Error::Transport(t)) => {
                bail!("no hub at {} ({t}). Run `switchboard doctor`.", self.url)
            }
        }
    }

    /// Requests that are safe to repeat are retried when the hub cannot be reached (a tunnel blip), within a total
    /// budget, so a hub that is down fails in seconds instead of hanging. A long poll (`wait=`) gets its wait on top.
    fn call(&self, method: &str, path: &str, body: Option<&Value>, retry: bool) -> Result<Reply> {
        let wait = path.split("wait=").nth(1).and_then(|w| w.split('&').next()).and_then(|w| w.parse::<f64>().ok())
            .unwrap_or(0.0);
        let per_try = Duration::from_secs_f64(10.0 + wait);
        let deadline = std::time::Instant::now() + Duration::from_secs_f64(if retry { 20.0 } else { 10.0 } + wait);
        let mut delay = Duration::from_millis(300);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.raw(method, path, body, per_try.min(left.max(Duration::from_secs(1)))) {
                Err(e) if retry && e.downcast_ref::<HubError>().is_none() && left > delay + Duration::from_secs(1) => {
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(Duration::from_secs(3));
                }
                other => return other,
            }
        }
    }

    fn ok<T: DeserializeOwned>(&self, method: &str, path: &str, body: Option<&Value>, retry: bool) -> Result<T> {
        match self.call(method, path, body, retry)? {
            Reply::Ok(v) => Ok(serde_json::from_value(v)?),
            Reply::NoContent => bail!("hub returned no content"),
            Reply::Conflict(v) => {
                let msg = v.get("error").and_then(|e| e.as_str()).map(String::from).unwrap_or_else(|| v.to_string());
                Err(anyhow!(HubError { status: 409, message: msg }))
            }
        }
    }

    pub fn health(&self) -> Result<Value> {
        self.ok("GET", "/healthz", None, false)
    }

    pub fn whoami(&self) -> Result<Value> {
        self.ok("GET", "/v1/whoami", None, true)
    }

    /// Sends get an idempotency key so a retried send never arrives twice.
    pub fn send(&self, m: &NewMsg) -> Result<Msg> {
        let mut m = m.clone();
        if m.idempotency_key.is_none() {
            m.idempotency_key = Some(crate::store::random_secret("k_"));
        }
        self.ok("POST", "/v1/messages", Some(&serde_json::to_value(&m)?), true)
    }

    pub fn inbox(&self, all: bool, wait_secs: f64) -> Result<Vec<Msg>> {
        self.ok("GET", &format!("/v1/inbox?all={all}&wait={wait_secs}"), None, true)
    }

    pub fn take(&self, lease_secs: Option<u64>) -> Result<Option<Msg>> {
        let body = json!({"lease_secs": lease_secs});
        match self.call("POST", "/v1/take", Some(&body), false)? {
            Reply::Ok(v) => Ok(Some(serde_json::from_value(v)?)),
            _ => Ok(None),
        }
    }

    pub fn renew(&self, id: i64, lease_secs: Option<u64>) -> Result<Msg> {
        self.ok("POST", &format!("/v1/messages/{id}/renew"), Some(&json!({"lease_secs": lease_secs})), true)
    }

    pub fn done(&self, id: i64, note: Option<&str>) -> Result<Msg> {
        self.ok("POST", &format!("/v1/messages/{id}/done"), Some(&json!({"note": note})), true)
    }

    pub fn release(&self, id: i64) -> Result<Msg> {
        self.ok("POST", &format!("/v1/messages/{id}/release"), None, true)
    }

    pub fn retry(&self, id: i64) -> Result<Msg> {
        self.ok("POST", &format!("/v1/messages/{id}/retry"), None, true)
    }

    pub fn thread(&self, id: i64) -> Result<Vec<Msg>> {
        self.ok("GET", &format!("/v1/messages/{id}"), None, true)
    }

    pub fn sent(&self) -> Result<Vec<Msg>> {
        self.ok("GET", "/v1/sent", None, true)
    }

    pub fn dead(&self) -> Result<Vec<Msg>> {
        self.ok("GET", "/v1/dead", None, true)
    }

    /// New messages past this consumer's cursor (long-polls up to `wait_secs`). Call `ack` after handling them.
    pub fn watch(&self, consumer: &str, wait_secs: f64) -> Result<(i64, Vec<Msg>)> {
        let v: Value = self.ok("GET", &format!("/v1/watch?consumer={consumer}&wait={wait_secs}"), None, true)?;
        Ok((v["cursor"].as_i64().unwrap_or(0), serde_json::from_value(v["messages"].clone())?))
    }

    pub fn ack(&self, consumer: &str, last_id: i64) -> Result<()> {
        let _: Value = self.ok("POST", "/v1/watch/ack", Some(&json!({"consumer": consumer, "last_id": last_id})),
                               true)?;
        Ok(())
    }

    pub fn locks(&self) -> Result<Vec<LockState>> {
        self.ok("GET", "/v1/locks", None, true)
    }

    pub fn lock_state(&self, name: &str) -> Result<LockState> {
        self.ok("GET", &format!("/v1/locks/{name}"), None, true)
    }

    /// (granted, state)
    pub fn lock_take(&self, name: &str, note: Option<&str>, ttl_secs: u64) -> Result<(bool, LockState)> {
        match self.call("POST", &format!("/v1/locks/{name}/take"),
                        Some(&json!({"note": note, "ttl_secs": ttl_secs})), false)? {
            Reply::Ok(v) => Ok((true, serde_json::from_value(v)?)),
            Reply::Conflict(v) => Ok((false, serde_json::from_value(v)?)),
            Reply::NoContent => bail!("unexpected empty reply"),
        }
    }

    pub fn lock_renew(&self, name: &str, ttl_secs: u64) -> Result<LockState> {
        self.ok("POST", &format!("/v1/locks/{name}/renew"), Some(&json!({"ttl_secs": ttl_secs})), true)
    }

    pub fn lock_release(&self, name: &str, note: Option<&str>, force: bool) -> Result<LockState> {
        self.ok("POST", &format!("/v1/locks/{name}/release"), Some(&json!({"note": note, "force": force})), true)
    }

    pub fn lock_check(&self, name: &str, fence: i64) -> Result<(bool, LockState)> {
        match self.call("GET", &format!("/v1/locks/{name}/check?fence={fence}"), None, true)? {
            Reply::Ok(v) | Reply::Conflict(v) => {
                Ok((v["valid"].as_bool().unwrap_or(false), serde_json::from_value(v["lock"].clone())?))
            }
            Reply::NoContent => bail!("unexpected empty reply"),
        }
    }

    pub fn invite(&self, prefix: &str, ttl_secs: u64) -> Result<Value> {
        self.ok("POST", "/v1/admin/invites", Some(&json!({"prefix": prefix, "ttl_secs": ttl_secs})), false)
    }

    pub fn enroll(&self, secret: &str, peer_key: &str) -> Result<Value> {
        self.ok("POST", "/v1/admin/enroll", Some(&json!({"secret": secret, "peer_key": peer_key})), false)
    }

    pub fn peers(&self) -> Result<Value> {
        self.ok("GET", "/v1/admin/peers", None, true)
    }

    pub fn remove_peer(&self, key: &str) -> Result<Value> {
        self.ok("POST", "/v1/admin/peers/remove", Some(&json!({"key": key})), false)
    }

    pub fn tokens(&self) -> Result<Value> {
        self.ok("GET", "/v1/admin/tokens", None, true)
    }

    pub fn create_token(&self, prefix: &str, label: &str, admin: bool) -> Result<Value> {
        self.ok("POST", "/v1/admin/tokens", Some(&json!({"prefix": prefix, "label": label, "admin": admin})), false)
    }

    pub fn revoke_token(&self, id: i64) -> Result<Value> {
        self.ok("POST", &format!("/v1/admin/tokens/{id}/revoke"), None, false)
    }
}
