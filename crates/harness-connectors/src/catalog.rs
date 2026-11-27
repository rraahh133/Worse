//! The built-in connector list (`assets/catalog.json`): official remote MCP servers run by the service itself.

use serde::{Deserialize, Serialize};

use crate::{ConnectorError, Result};

const CATALOG: &str = include_str!("../assets/catalog.json");

/// How a server is signed in to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Auth {
    /// MCP OAuth (the default).
    #[default]
    OAuth,
    /// Public server, no sign-in.
    None,
}

/// Where the OAuth client comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Client {
    /// Registered on the fly (dynamic client registration).
    #[default]
    Dcr,
    /// The service does not register clients on the fly: the user brings an OAuth client id of their own.
    Own,
}

/// One connector of the catalog (or a custom one the user added).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub publisher: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub category: String,
    pub url: String,
    #[serde(default)]
    pub homepage: String,
    #[serde(default)]
    pub auth: Auth,
    #[serde(default)]
    pub client: Client,
}

/// The built-in catalog, sorted by title.
#[must_use]
pub fn builtin() -> Vec<Entry> {
    let mut entries: Vec<Entry> = serde_json::from_str(CATALOG).unwrap_or_default();
    entries.sort_by_key(|e| e.title.to_lowercase());
    entries
}

/// A catalog entry by id.
#[must_use]
pub fn get(id: &str) -> Option<Entry> {
    builtin().into_iter().find(|e| e.id == id)
}

/// Checks a server URL the user typed: https (or http on loopback), no credentials in it.
///
/// # Errors
///
/// [`ConnectorError::Invalid`] with the reason.
pub fn check_url(url: &str) -> Result<reqwest::Url> {
    let parsed = reqwest::Url::parse(url.trim())
        .map_err(|_| ConnectorError::Invalid(format!("{url} is not a valid URL")))?;
    let loopback = matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    if parsed.scheme() != "https" && !(parsed.scheme() == "http" && loopback) {
        return Err(ConnectorError::Invalid(
            "connector URLs must use https".to_owned(),
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(ConnectorError::Invalid(
            "put no user name or password in the URL".to_owned(),
        ));
    }
    Ok(parsed)
}

/// An id for a custom connector from its URL host (`mcp.example.com` -> `example`), unique against `taken`.
#[must_use]
pub fn custom_id(url: &reqwest::Url, taken: &[String]) -> String {
    let host = url.host_str().unwrap_or("custom");
    let parts: Vec<&str> = host
        .split('.')
        .filter(|p| {
            !matches!(
                *p,
                "mcp" | "api" | "www" | "com" | "io" | "app" | "dev" | "ai"
            )
        })
        .collect();
    let base: String = parts
        .first()
        .copied()
        .unwrap_or("custom")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let base = if base.is_empty() {
        "custom".to_owned()
    } else {
        base
    };
    let mut id = base.clone();
    let mut n = 2;
    while taken.contains(&id) || get(&id).is_some() {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}
