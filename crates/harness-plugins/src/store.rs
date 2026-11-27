//! `<HARNESS_HOME>/plugins/marketplaces.json`: the marketplaces the store lists, and where they are checked out.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{PluginError, Result};
use crate::marketplace::{self, check_name, github_url, Marketplace, MARKETPLACE_FILE};
use crate::record::plugins_dir;

/// The marketplace every store starts with.
pub const DEFAULT_MARKETPLACE: &str = "anthropics/claude-plugins-official";

/// One configured marketplace: `owner/repo`, an `https://` git URL, or a local folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceSource {
    pub source: String,
    /// Name from its `marketplace.json`, known after the first fetch.
    #[serde(default)]
    pub name: Option<String>,
}

impl MarketplaceSource {
    #[must_use]
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            name: None,
        }
    }

    /// A local folder (absolute path that exists), read in place instead of downloaded.
    #[must_use]
    pub fn local_dir(&self) -> Option<PathBuf> {
        let path = Path::new(self.source.trim());
        (path.is_absolute() && path.is_dir()).then(|| path.to_path_buf())
    }

    /// The git URL to download from.
    ///
    /// # Errors
    ///
    /// Neither `owner/repo` nor an `https://` URL.
    pub fn git_url(&self) -> Result<String> {
        let s = self.source.trim();
        if s.starts_with("https://") {
            marketplace::check_url(s)?;
            return Ok(s.to_owned());
        }
        github_url(s)
    }

    /// Folder name of the checkout: a safe slug of the source.
    #[must_use]
    pub fn slug(&self) -> String {
        let slug: String = self
            .source
            .trim()
            .trim_start_matches("https://")
            .trim_end_matches(".git")
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        slug.trim_matches('_').chars().take(96).collect()
    }
}

/// Where marketplace `source` is checked out (or its own folder when local).
#[must_use]
pub fn checkout_dir(data_dir: &Path, source: &MarketplaceSource) -> PathBuf {
    source.local_dir().unwrap_or_else(|| {
        plugins_dir(data_dir)
            .join("marketplaces")
            .join(source.slug())
    })
}

/// Download folder of a plugin from another repository.
///
/// # Errors
///
/// Unsafe marketplace or plugin name.
pub fn cache_dir(data_dir: &Path, marketplace: &str, plugin: &str) -> Result<PathBuf> {
    check_name(marketplace)?;
    check_name(plugin)?;
    Ok(plugins_dir(data_dir)
        .join("cache")
        .join(marketplace)
        .join(plugin))
}

/// Configured marketplaces (the default one when the file is missing).
pub async fn load(data_dir: &Path) -> Vec<MarketplaceSource> {
    let path = plugins_dir(data_dir).join("marketplaces.json");
    match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => vec![MarketplaceSource::new(DEFAULT_MARKETPLACE)],
    }
}

/// Writes the marketplace list atomically.
///
/// # Errors
///
/// I/O.
pub async fn save(data_dir: &Path, list: &[MarketplaceSource]) -> Result<()> {
    let path = plugins_dir(data_dir).join("marketplaces.json");
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut text = serde_json::to_string_pretty(list).unwrap_or_else(|_| "[]".to_owned());
    text.push('\n');
    harness_settings::fsx::write_atomic(&path, text.as_bytes()).await?;
    Ok(())
}

/// Reads the marketplace file of a checkout.
///
/// # Errors
///
/// Missing or invalid `marketplace.json`.
pub async fn read_marketplace(dir: &Path) -> Result<Marketplace> {
    let path = dir.join(MARKETPLACE_FILE);
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| PluginError::invalid(format!("{}: {e}", path.display())))?;
    marketplace::parse(&bytes)
}
