//! MCP server definitions in JSON files (Claude Code, agy, Harness overlay) and Codex `config.toml`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use harness_settings::fsx;
use harness_settings::json::OMap;
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::error::{ConfigError, Result};
use crate::jsonfile::{
    extra_of, get_bool, get_list, get_map, get_str, set_bool, set_extra, set_list, set_map,
    set_str, JsonFile,
};
use crate::plan::{FileChange, Kind};
use crate::types::{McpServer, McpTransport};

mod claude_enabled;
mod codex_toml;

pub(crate) use claude_enabled::{claude_project_enabled, plan_claude_project_enabled};
pub(crate) use codex_toml::{list_toml, plan_delete_toml, plan_enabled_toml, plan_upsert_toml};

/// JSON flavours of `mcpServers` maps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dialect {
    /// `~/.claude.json`, `<ws>/.mcp.json`: `type`, `command`, `args`, `env`, `url`, `headers`.
    Claude,
    /// agy `mcp_config.json`: `command`, `args`, `env`, `serverUrl`|`url`, `headers`, `disabled`.
    Agy,
    /// Harness `mcp.json`: Claude keys plus `enabled`, `agents`.
    Overlay,
}

impl Dialect {
    fn known(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &["type", "command", "args", "env", "url", "headers"],
            Self::Agy => &[
                "command",
                "args",
                "env",
                "serverUrl",
                "url",
                "headers",
                "disabled",
            ],
            Self::Overlay => &[
                "type", "command", "args", "env", "url", "headers", "enabled", "agents",
            ],
        }
    }
}

pub(crate) const MCP_KEY: &str = "mcpServers";

/// Validates a server before writing.
pub(crate) fn validate(server: &McpServer) -> Result<()> {
    if server.name.trim().is_empty() || server.name.contains(['.', '/', '\\', '"']) {
        return Err(ConfigError::invalid(format!(
            "invalid server name {:?}",
            server.name
        )));
    }
    match server.transport {
        McpTransport::Stdio
            if server
                .command
                .as_deref()
                .map_or(true, |c| c.trim().is_empty()) =>
        {
            Err(ConfigError::invalid("a stdio server needs a command"))
        }
        McpTransport::Http | McpTransport::Sse
            if server.url.as_deref().map_or(true, |u| u.trim().is_empty()) =>
        {
            Err(ConfigError::invalid("an http/sse server needs a url"))
        }
        _ => Ok(()),
    }
}

fn transport_str(t: McpTransport) -> &'static str {
    match t {
        McpTransport::Stdio => "stdio",
        McpTransport::Http => "http",
        McpTransport::Sse => "sse",
    }
}

fn read_entry(name: &str, map: &OMap, dialect: Dialect) -> McpServer {
    let url = get_str(map, "url").or_else(|| {
        (dialect == Dialect::Agy)
            .then(|| get_str(map, "serverUrl"))
            .flatten()
    });
    let transport = match get_str(map, "type").as_deref() {
        Some("http" | "streamable-http" | "streamableHttp") => McpTransport::Http,
        Some("sse") => McpTransport::Sse,
        Some(_) => McpTransport::Stdio,
        None if url.is_some() => McpTransport::Http,
        None => McpTransport::Stdio,
    };
    McpServer {
        name: name.to_owned(),
        transport,
        command: get_str(map, "command"),
        args: get_list(map, "args"),
        env: get_map(map, "env"),
        url,
        headers: get_map(map, "headers"),
        enabled: match dialect {
            Dialect::Claude => None,
            Dialect::Agy => Some(!get_bool(map, "disabled").unwrap_or(false)),
            Dialect::Overlay => Some(get_bool(map, "enabled").unwrap_or(true)),
        },
        startup_timeout_sec: None,
        tool_timeout_sec: None,
        agents: if dialect == Dialect::Overlay {
            get_list(map, "agents")
        } else {
            Vec::new()
        },
        extra: extra_of(map, dialect.known()),
    }
}

fn write_entry(map: &mut OMap, server: &McpServer, dialect: Dialect, is_new: bool) {
    let stdio = server.transport == McpTransport::Stdio;
    if dialect != Dialect::Agy && (!stdio || is_new || map.get("type").is_some()) {
        set_str(map, "type", Some(transport_str(server.transport)));
    }
    set_str(map, "command", server.command.as_deref().filter(|_| stdio));
    set_list(map, "args", if stdio { &server.args } else { &[] });
    if !stdio {
        map.remove("args");
    }
    set_map(map, "env", &server.env);
    let url_key = if dialect == Dialect::Agy && (map.get("serverUrl").is_some() || is_new) {
        "serverUrl"
    } else {
        "url"
    };
    set_str(map, url_key, server.url.as_deref().filter(|_| !stdio));
    let no_headers = BTreeMap::new();
    set_map(
        map,
        "headers",
        if stdio { &no_headers } else { &server.headers },
    );
    match dialect {
        Dialect::Claude => {}
        Dialect::Agy => {
            let disabled = server.enabled == Some(false);
            if disabled || map.get("disabled").is_some() {
                set_bool(map, "disabled", Some(disabled));
            }
        }
        Dialect::Overlay => {
            set_bool(map, "enabled", Some(server.enabled.unwrap_or(true)));
            set_list(map, "agents", &server.agents);
        }
    }
    set_extra(map, dialect.known(), &server.extra);
}

/// Servers of an `mcpServers` object as plugins ship it (Claude Code keys; anything else = none).
pub(crate) fn from_value(servers: &serde_json::Value) -> Vec<McpServer> {
    let value = harness_settings::json::OValue::from_json(servers.clone());
    value
        .as_object()
        .map(|map| {
            map.iter()
                .filter_map(|(name, v)| v.as_object().map(|m| read_entry(name, m, Dialect::Claude)))
                .collect()
        })
        .unwrap_or_default()
}

/// Servers of a JSON file (missing file = none).
pub(crate) async fn list_json(path: &Path, dialect: Dialect) -> Result<Vec<McpServer>> {
    let file = JsonFile::load(path).await?;
    Ok(file
        .object(MCP_KEY)
        .map(|servers| {
            servers
                .iter()
                .filter_map(|(name, v)| v.as_object().map(|m| read_entry(name, m, dialect)))
                .collect()
        })
        .unwrap_or_default())
}

/// Creates or updates `server` (renaming from `previous_name`).
pub(crate) async fn plan_upsert_json(
    path: &Path,
    dialect: Dialect,
    server: &McpServer,
    previous_name: Option<&str>,
) -> Result<FileChange> {
    validate(server)?;
    let mut file = JsonFile::load(path).await?;
    let servers = file.root().object_mut(MCP_KEY);
    if let Some(previous) = previous_name.filter(|p| *p != server.name) {
        if servers.get(&server.name).is_some() {
            return Err(ConfigError::invalid(format!(
                "a server named {} already exists",
                server.name
            )));
        }
        if servers.get(previous).is_none() {
            return Err(ConfigError::NotFound(previous.to_owned()));
        }
        servers.rename(previous, &server.name);
    }
    let is_new = servers.get(&server.name).is_none();
    let entry = servers.object_mut(&server.name);
    write_entry(entry, server, dialect, is_new);
    Ok(file.into_change())
}

pub(crate) async fn plan_add_many_json(
    path: &Path,
    dialect: Dialect,
    servers: &[McpServer],
) -> Result<FileChange> {
    let mut file = JsonFile::load(path).await?;
    let map = file.root().object_mut(MCP_KEY);
    for server in servers {
        validate(server)?;
        let is_new = map.get(&server.name).is_none();
        write_entry(map.object_mut(&server.name), server, dialect, is_new);
    }
    Ok(file.into_change())
}

/// One change that writes `upserts` and removes `removes` (missing names are ignored), so several servers of
/// one plugin end up in a single write instead of overwriting each other.
pub(crate) async fn plan_batch_json(
    path: &Path,
    dialect: Dialect,
    upserts: &[McpServer],
    removes: &[String],
) -> Result<FileChange> {
    let mut file = JsonFile::load(path).await?;
    let map = file.root().object_mut(MCP_KEY);
    for name in removes {
        map.remove(name);
    }
    for server in upserts {
        validate(server)?;
        let is_new = map.get(&server.name).is_none();
        write_entry(map.object_mut(&server.name), server, dialect, is_new);
    }
    Ok(file.into_change())
}

pub(crate) async fn plan_delete_json(path: &Path, name: &str) -> Result<FileChange> {
    let mut file = JsonFile::load(path).await?;
    let servers = file.root().object_mut(MCP_KEY);
    if servers.remove(name).is_none() {
        return Err(ConfigError::NotFound(name.to_owned()));
    }
    Ok(file.into_change())
}

pub(crate) async fn plan_enabled_json(
    path: &Path,
    dialect: Dialect,
    name: &str,
    enabled: bool,
) -> Result<FileChange> {
    let mut servers = list_json(path, dialect).await?;
    let mut server = servers
        .iter_mut()
        .find(|s| s.name == name)
        .map(std::mem::take)
        .ok_or_else(|| ConfigError::NotFound(name.to_owned()))?;
    server.enabled = Some(enabled);
    plan_upsert_json(path, dialect, &server, None).await
}

#[cfg(test)]
mod tests;
