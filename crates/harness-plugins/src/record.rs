//! `<HARNESS_HOME>/plugins/installed.json`: which plugins are installed and what they wrote for which agent,
//! so uninstall and disable remove exactly that. Disk stays the truth for whether an entry is present.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use harness_agent_config::{AgentKind, Concept};
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// One configuration entry a plugin wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Written {
    pub agent: AgentKind,
    pub concept: Concept,
    pub name: String,
}

/// One installed plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InstalledPlugin {
    pub marketplace: String,
    pub version: Option<String>,
    /// Commit the files came from.
    pub rev: Option<String>,
    /// Folder the plugin was read from (MCP commands may point into it).
    pub root: PathBuf,
    pub enabled: bool,
    pub written: Vec<Written>,
}

/// The installed record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Installed {
    pub plugins: BTreeMap<String, InstalledPlugin>,
}

/// `<data dir>/plugins`.
#[must_use]
pub fn plugins_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("plugins")
}

fn file(data_dir: &Path) -> PathBuf {
    plugins_dir(data_dir).join("installed.json")
}

impl Installed {
    /// Reads the record; a missing or unreadable file is an empty record.
    pub async fn load(data_dir: &Path) -> Self {
        match tokio::fs::read(file(data_dir)).await {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!("plugins/installed.json unreadable, starting empty: {e}");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Writes the record atomically.
    ///
    /// # Errors
    ///
    /// I/O.
    pub async fn save(&self, data_dir: &Path) -> Result<()> {
        let path = file(data_dir);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut text = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_owned());
        text.push('\n');
        harness_settings::fsx::write_atomic(&path, text.as_bytes()).await?;
        Ok(())
    }
}
