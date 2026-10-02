//! Where switchboard keeps its files, and the one config file that says what this machine is.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const DEFAULT_PORT: u16 = 5490;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `$SWITCHBOARD_HOME`, else `~/.switchboard`.
pub fn home() -> PathBuf {
    if let Some(h) = std::env::var_os("SWITCHBOARD_HOME") {
        return PathBuf::from(h);
    }
    user_home().join(".switchboard")
}

pub fn user_home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// True when the home is the default one; services and PATH links are only named plainly for it.
pub fn is_default_home() -> bool {
    std::env::var_os("SWITCHBOARD_HOME").is_none()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    /// "hub" or "peer".
    pub role: String,
    /// The mailbox this machine acts as by default (`sean`, `tony`); agents add `/topic`.
    pub name: String,
    /// The hub's HTTP address as seen from this machine (always loopback).
    pub url: String,
    /// Bearer token; scoped to `name`'s prefix, or `*` on the hub.
    pub token: String,
    /// Absolute path of the tailcat binary, found at setup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailcat: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hub: Option<HubConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<PeerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubConfig {
    pub port: u16,
    /// Serve the hub to enrolled peers over tailcat.
    pub tailcat: bool,
    /// Refuse new messages when the disk has less than this free.
    pub min_free_mb: u64,
    /// Daily `VACUUM INTO` backups kept.
    pub backups_keep: u32,
    /// Default lease on `take`.
    pub lease_secs: u64,
    /// Takes whose lease ran out before a message goes to the dead-letter list.
    pub max_attempts: u32,
    /// Lock name -> mailboxes told whenever it is taken, released or expires.
    #[serde(default)]
    pub lock_notify: BTreeMap<String, Vec<String>>,
}

impl Default for HubConfig {
    fn default() -> Self {
        HubConfig {
            port: DEFAULT_PORT,
            tailcat: true,
            min_free_mb: 512,
            backups_keep: 7,
            lease_secs: 900,
            max_attempts: 5,
            lock_notify: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerConfig {
    /// The hub's tailcat address (a secret).
    pub hub_addr: String,
    pub hub_port: u16,
    /// Local port the tailcat forward listens on.
    pub local_port: u16,
}

pub fn config_path() -> PathBuf {
    home().join("config.toml")
}

impl Config {
    pub fn load() -> Result<Config> {
        let path = config_path();
        let text = std::fs::read_to_string(&path).with_context(|| {
            format!("no switchboard config at {} -- run `switchboard setup` (hub) or `switchboard join <code>` (peer)",
                    path.display())
        })?;
        let cfg: Config = toml::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
        Ok(cfg)
    }

    /// The config, or an empty one, with `SWITCHBOARD_URL` / `SWITCHBOARD_TOKEN` applied on top.
    pub fn load_for_client() -> Result<Config> {
        let mut cfg = match Config::load() {
            Ok(c) => c,
            Err(e) => {
                if std::env::var("SWITCHBOARD_URL").is_err() {
                    return Err(e);
                }
                Config::default()
            }
        };
        if let Ok(u) = std::env::var("SWITCHBOARD_URL") {
            cfg.url = u;
        }
        if let Ok(t) = std::env::var("SWITCHBOARD_TOKEN") {
            cfg.token = t;
        }
        if cfg.url.is_empty() {
            bail!("no hub url configured");
        }
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let dir = home();
        std::fs::create_dir_all(&dir)?;
        let path = config_path();
        let text = toml::to_string_pretty(self)?;
        write_private(&path, text.as_bytes())?;
        Ok(())
    }

    pub fn tailcat_bin(&self) -> String {
        self.tailcat.clone().unwrap_or_else(|| "tailcat".into())
    }
}

/// Write a file only the owner can read (tokens live in it).
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// "15m", "24h", "30s", "2d" or plain seconds.
pub fn parse_duration(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        Some('d') => (&s[..s.len() - 1], 86400),
        _ => (s, 1),
    };
    let n: u64 = num.parse().with_context(|| format!("bad duration {s:?} (try 30s, 15m, 24h)"))?;
    Ok(n * mult)
}

/// Mailbox names: `sean`, `tony/merge`, `tony/worldgen.2`.
pub fn valid_box(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 120
        && name.split('/').all(|seg| {
            !seg.is_empty() && seg.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
}

/// A destination: a mailbox, `person/*` (everyone under person), or `*` (everyone).
pub fn valid_destination(to: &str) -> bool {
    to == "*" || to.strip_suffix("/*").map(valid_box).unwrap_or(false) || valid_box(to)
}

/// Whether a token scoped to `prefix` may act as `name`.
pub fn prefix_allows(prefix: &str, name: &str) -> bool {
    prefix == "*" || name == prefix || name.starts_with(&format!("{prefix}/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(valid_box("tony/merge"));
        assert!(!valid_box("tony//merge"));
        assert!(!valid_box("tony/*"));
        assert!(valid_destination("tony/*"));
        assert!(valid_destination("*"));
        assert!(prefix_allows("tony", "tony/merge"));
        assert!(!prefix_allows("tony", "tonya"));
        assert!(prefix_allows("*", "anything"));
        assert_eq!(parse_duration("15m").unwrap(), 900);
        assert_eq!(parse_duration("90").unwrap(), 90);
    }
}
