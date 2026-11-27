//! `.claude-plugin/marketplace.json`: the plugin list of one marketplace.
//!
//! Parsing is lenient per entry (a broken entry is skipped, the rest stays usable) and strict about
//! anything that becomes a path or a download URL.

use serde_json::{Map, Value};

use crate::error::{PluginError, Result};

/// Marketplace file inside a marketplace checkout.
pub const MARKETPLACE_FILE: &str = ".claude-plugin/marketplace.json";
/// Longest plugin name accepted (it becomes a folder name).
const MAX_NAME_LEN: usize = 96;

/// Where a plugin's files come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A folder inside the marketplace checkout (`"./plugins/x"`).
    Relative(String),
    /// A git repository (`url`, `github`, `git-subdir` sources): `rev` is the pinned sha or else the ref,
    /// `path` the plugin folder inside the repository.
    Git {
        url: String,
        rev: Option<String>,
        path: Option<String>,
    },
    /// A source Harness does not install (`npm`, `command`, …); the text says why.
    Unsupported(String),
}

/// One plugin entry of a marketplace.
#[derive(Debug, Clone, PartialEq)]
pub struct Listing {
    pub name: String,
    pub display_name: Option<String>,
    pub description: String,
    pub category: Option<String>,
    pub author: Option<String>,
    pub homepage: Option<String>,
    pub version: Option<String>,
    pub source: Source,
    /// `strict: false`: the entry itself may declare the components (no `plugin.json` needed).
    pub strict: bool,
    /// Component declarations carried by the entry (`skills`, `commands`, `agents`, `mcpServers`,
    /// `lspServers`, `hooks`), read the same way as in `plugin.json`.
    pub components: Map<String, Value>,
}

impl Listing {
    /// Name shown in the store.
    #[must_use]
    pub fn title(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }
}

/// A parsed marketplace.
#[derive(Debug, Clone, PartialEq)]
pub struct Marketplace {
    pub name: String,
    pub description: Option<String>,
    pub plugins: Vec<Listing>,
    /// Entries that could not be read: `(name or index, reason)`.
    pub skipped: Vec<(String, String)>,
}

impl Marketplace {
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Listing> {
        self.plugins.iter().find(|p| p.name == name)
    }
}

/// Keys of a plugin entry that declare components.
pub(crate) const COMPONENT_KEYS: [&str; 6] = [
    "skills",
    "commands",
    "agents",
    "mcpServers",
    "lspServers",
    "hooks",
];

/// Parses `marketplace.json`.
///
/// # Errors
///
/// Not JSON, no `name`, or no `plugins` array.
pub fn parse(bytes: &[u8]) -> Result<Marketplace> {
    let root: Value = serde_json::from_slice(bytes)
        .map_err(|e| PluginError::invalid(format!("marketplace.json: {e}")))?;
    let name =
        str_of(&root, "name").ok_or_else(|| PluginError::invalid("marketplace has no name"))?;
    check_name(&name)?;
    let entries = root
        .get("plugins")
        .and_then(Value::as_array)
        .ok_or_else(|| PluginError::invalid("marketplace has no plugins list"))?;
    let mut plugins = Vec::new();
    let mut skipped = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        match listing(entry) {
            Ok(l) if plugins.iter().any(|p: &Listing| p.name == l.name) => {
                skipped.push((l.name, "duplicate name".to_owned()));
            }
            Ok(l) => plugins.push(l),
            Err(e) => skipped.push((
                str_of(entry, "name").unwrap_or_else(|| format!("#{i}")),
                e.to_string(),
            )),
        }
    }
    Ok(Marketplace {
        name,
        description: str_of(&root, "description"),
        plugins,
        skipped,
    })
}

fn listing(entry: &Value) -> Result<Listing> {
    let name = str_of(entry, "name").ok_or_else(|| PluginError::invalid("entry has no name"))?;
    check_name(&name)?;
    let source = source(entry.get("source").unwrap_or(&Value::Null))?;
    let components = entry
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(k, _)| COMPONENT_KEYS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    Ok(Listing {
        display_name: str_of(entry, "displayName"),
        description: str_of(entry, "description").unwrap_or_default(),
        category: str_of(entry, "category"),
        author: entry
            .get("author")
            .and_then(|a| str_of(a, "name").or_else(|| a.as_str().map(str::to_owned))),
        homepage: str_of(entry, "homepage"),
        version: str_of(entry, "version"),
        strict: entry.get("strict").and_then(Value::as_bool).unwrap_or(true),
        source,
        components,
        name,
    })
}

fn source(value: &Value) -> Result<Source> {
    if let Some(path) = value.as_str() {
        return relative(path).map(Source::Relative);
    }
    let kind = str_of(value, "source").unwrap_or_default();
    let rev = str_of(value, "sha").or_else(|| str_of(value, "ref"));
    if let Some(rev) = &rev {
        check_rev(rev)?;
    }
    let path = str_of(value, "path").map(|p| relative(&p)).transpose()?;
    let url = match kind.as_str() {
        "github" => {
            let repo = str_of(value, "repo")
                .ok_or_else(|| PluginError::invalid("github source has no repo"))?;
            github_url(&repo)?
        }
        "url" | "git" | "git-subdir" => {
            let url =
                str_of(value, "url").ok_or_else(|| PluginError::invalid("source has no url"))?;
            check_url(&url)?;
            url
        }
        "" => return Err(PluginError::invalid("entry has no source")),
        other => {
            return Ok(Source::Unsupported(format!(
                "{other} sources are not supported"
            )))
        }
    };
    Ok(Source::Git { url, rev, path })
}

/// `owner/repo` → its GitHub clone URL.
///
/// # Errors
///
/// Not exactly two safe segments.
pub fn github_url(repo: &str) -> Result<String> {
    let parts: Vec<&str> = repo.trim().trim_end_matches(".git").split('/').collect();
    let ok = parts.len() == 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && !p.starts_with('.')
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        });
    if !ok {
        return Err(PluginError::invalid(format!(
            "not a GitHub owner/repo: {repo:?}"
        )));
    }
    Ok(format!("https://github.com/{}/{}.git", parts[0], parts[1]))
}

/// Download URLs must be plain `https://` (no credentials, no other transports git would run).
pub(crate) fn check_url(url: &str) -> Result<()> {
    let rest = url.strip_prefix("https://").ok_or_else(|| {
        PluginError::invalid(format!("only https:// sources are installed: {url:?}"))
    })?;
    let host = rest.split('/').next().unwrap_or_default();
    if host.is_empty()
        || host.contains('@')
        || url.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(PluginError::invalid(format!("unsafe source url {url:?}")));
    }
    Ok(())
}

/// A sha or ref name; never something git would read as an option.
pub(crate) fn check_rev(rev: &str) -> Result<()> {
    let ok = !rev.is_empty()
        && !rev.starts_with('-')
        && !rev.contains("..")
        && rev
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c));
    if ok {
        Ok(())
    } else {
        Err(PluginError::invalid(format!("unsafe git revision {rev:?}")))
    }
}

/// A relative path inside a checkout (`./a/b` → `a/b`); `..`, absolute paths and drive letters are refused.
pub(crate) fn relative(path: &str) -> Result<String> {
    let trimmed = path.trim().trim_start_matches("./").trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "." {
        return Ok(String::new());
    }
    let bad = trimmed.starts_with('/')
        || trimmed.contains('\\')
        || trimmed.contains(':')
        || trimmed
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..");
    if bad {
        return Err(PluginError::invalid(format!("unsafe path {path:?}")));
    }
    Ok(trimmed.to_owned())
}

/// Plugin and marketplace names become folder names.
pub(crate) fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    if ok {
        Ok(())
    } else {
        Err(PluginError::invalid(format!("unsafe name {name:?}")))
    }
}

fn str_of(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
