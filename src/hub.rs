//! Starting a hub: the store, the API on loopback, and (optionally) the tailcat tunnel.

use crate::config::{home, Config, HubConfig};
use crate::server::{AppState, HubOptions};
use crate::store::{Store, StoreOptions};
use crate::tailcat;
use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Notify;

pub fn db_path(dir: &Path) -> std::path::PathBuf {
    dir.join("switchboard.sqlite")
}

pub fn open_store(dir: &Path, hc: &HubConfig) -> Result<Arc<Store>> {
    std::fs::create_dir_all(dir)?;
    Ok(Arc::new(Store::open(&db_path(dir), StoreOptions {
        max_attempts: hc.max_attempts,
        lock_notify: hc.lock_notify.clone(),
    })?))
}

/// Build the hub's state; with `tunnel`, also start the tailcat supervisor (needs a tokio runtime).
pub fn state(dir: &Path, hc: &HubConfig, tailcat_bin: Option<String>) -> Result<AppState> {
    let store = open_store(dir, hc)?;
    let mut opts = HubOptions {
        data_dir: dir.to_path_buf(),
        min_free_mb: hc.min_free_mb,
        lease_secs: hc.lease_secs,
        backups_keep: hc.backups_keep,
        on_peers_changed: None,
        on_invite: None,
        hub_addr: None,
        port: hc.port,
    };
    if let Some(bin) = tailcat_bin {
        let t = tailcat::spawn_hub_tunnel(store.clone(), dir.to_path_buf(), bin, hc.port);
        let (pc, im) = (t.peers_changed.clone(), t.invite_made.clone());
        opts.on_peers_changed = Some(Box::new(move || pc.notify_one()));
        opts.on_invite = Some(Box::new(move || im.notify_one()));
        let addr_file = tailcat::hub_addr_file(dir);
        opts.hub_addr = Some(Box::new(move || tailcat::read_addr(&addr_file)));
    }
    Ok(AppState { store, notify: Arc::new(Notify::new()), opts: Arc::new(opts), started: Instant::now() })
}

/// `switchboard serve` / the hub's daemon: run until killed.
pub fn serve_forever(cfg: &Config, port_override: Option<u16>, no_tailcat: bool) -> Result<()> {
    let mut hc = cfg.hub.clone().unwrap_or_default();
    if let Some(p) = port_override {
        hc.port = p;
    }
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let bin = if hc.tailcat && !no_tailcat { Some(cfg.tailcat_bin()) } else { None };
        let st = state(&home(), &hc, bin.clone())?;
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", hc.port))
            .await
            .with_context(|| format!("port {} is busy -- is another hub running? (`switchboard doctor`)", hc.port))?;
        eprintln!("switchboard {}: hub on http://127.0.0.1:{}, store {}{}", crate::config::VERSION, hc.port,
                  db_path(&home()).display(), if bin.is_some() { ", tailcat on" } else { ", tailcat off" });
        crate::server::run(listener, st).await
    })
}
