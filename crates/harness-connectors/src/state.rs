//! Connector files under `<HARNESS_HOME>/connectors/`: `connected.json` (written by the app only) and
//! `tokens.json` (written by the app at sign-in and by bridges when they refresh, under `tokens.lock`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::catalog::{Auth, Entry};
use crate::Result;

const CONNECTED_FILE: &str = "connected.json";
const TOKENS_FILE: &str = "tokens.json";
const LOCK_FILE: &str = "tokens.lock";

/// One connected connector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub auth: Auth,
    /// Switched off: kept (with its sign-in) but not given to agents.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Added by the user, not from the catalog: the entry itself lives here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<Entry>,
}

fn yes() -> bool {
    true
}

/// `connected.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connected {
    #[serde(default)]
    pub connectors: BTreeMap<String, Connection>,
}

/// OAuth state of one connector. Secret: never logged, never sent to the UI or an agent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Token {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    pub token_endpoint: String,
    /// RFC 8707 resource indicator sent with token requests.
    #[serde(default)]
    pub resource: String,
    #[serde(default)]
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds; `None` = unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

impl Token {
    /// True when the access token is missing or expires within a minute.
    #[must_use]
    pub fn stale(&self, now: u64) -> bool {
        self.access_token.is_empty() || self.expires_at.is_some_and(|at| at <= now + 60)
    }
}

/// `tokens.json`, plus OAuth clients the user brought for services without on-the-fly registration
/// (keyed by authorization server, so one Google client serves Drive, Gmail and the rest).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    #[serde(default)]
    pub tokens: BTreeMap<String, Token>,
    #[serde(default)]
    pub clients: BTreeMap<String, OwnClient>,
}

/// An OAuth client the user registered with a service.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnClient {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
}

fn path(home: &Path, name: &str) -> PathBuf {
    crate::dir(home).join(name)
}

async fn read<T: Default + for<'de> Deserialize<'de>>(file: &Path) -> T {
    match tokio::fs::read(file).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            tracing::warn!(%error, file = %file.display(), "ignoring unreadable connector file");
            T::default()
        }),
        Err(_) => T::default(),
    }
}

async fn write<T: Serialize>(file: &Path, value: &T) -> Result<()> {
    if let Some(dir) = file.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    let temp = file.with_extension(format!("tmp{}", std::process::id()));
    tokio::fs::write(&temp, bytes).await?;
    tokio::fs::rename(&temp, file).await?;
    Ok(())
}

impl Connected {
    pub async fn load(home: &Path) -> Self {
        read(&path(home, CONNECTED_FILE)).await
    }

    /// # Errors
    ///
    /// Writing the file failed.
    pub async fn save(&self, home: &Path) -> Result<()> {
        write(&path(home, CONNECTED_FILE), self).await
    }
}

impl Tokens {
    pub async fn load(home: &Path) -> Self {
        read(&path(home, TOKENS_FILE)).await
    }

    /// # Errors
    ///
    /// Writing the file failed.
    pub async fn save(&self, home: &Path) -> Result<()> {
        write(&path(home, TOKENS_FILE), self).await
    }

    /// Changes one connector's token under the cross-process lock (bridges refresh concurrently, and a refresh
    /// token may be single-use).
    ///
    /// # Errors
    ///
    /// Writing the file failed.
    pub async fn update(home: &Path, f: impl FnOnce(&mut Self)) -> Result<()> {
        let _lock = Lock::acquire(home).await;
        let mut tokens = Self::load(home).await;
        f(&mut tokens);
        tokens.save(home).await
    }
}

/// `tokens.lock`: created exclusively, removed on drop; a lock older than 30 s is taken over.
pub struct Lock(PathBuf);

impl Lock {
    pub async fn acquire(home: &Path) -> Option<Self> {
        let file = path(home, LOCK_FILE);
        let _ = tokio::fs::create_dir_all(crate::dir(home)).await;
        for _ in 0..100 {
            if std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&file)
                .is_ok()
            {
                return Some(Self(file));
            }
            let old = std::fs::metadata(&file)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(30));
            if old {
                let _ = std::fs::remove_file(&file);
                continue;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        None
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Unix seconds now.
#[must_use]
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
