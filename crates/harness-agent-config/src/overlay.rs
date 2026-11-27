//! Harness MCP overlay (spec §9.1.4): `<data dir>/mcp.json`, appended to the ACP `mcpServers` of every
//! `session/new` and `session/load`. This module is the only parser of the file (the hub calls it).

use std::path::Path;

use serde_json::{json, Value};

use crate::error::Result;
use crate::mcp::{list_json, Dialect};
use crate::types::{McpServer, McpTransport};

/// File name inside the data dir.
pub const OVERLAY_FILE: &str = "mcp.json";
/// Reserved name of the built-in memory server (overlay entries with this name are ignored).
pub const MEMORY_SERVER: &str = "harness-memory";
/// Reserved name of the tool gateway that fronts overlay servers for agents that cannot take many tools.
pub const GATEWAY_SERVER: &str = "harness-tools";
/// Built-in Office documents server.
pub const WORK_SERVER: &str = "harness-work";
/// Built-in computer-use server.
pub const COMPUTER_SERVER: &str = "harness-computer";
pub const BUILTIN_SERVERS: [&str; 5] = [
    MEMORY_SERVER,
    "harness-delegate",
    COMPUTER_SERVER,
    GATEWAY_SERVER,
    WORK_SERVER,
];
/// Built-in servers the user can switch off as plugins (`docs/plans/plugins.md` §3).
pub const SWITCHABLE_BUILTINS: [&str; 2] = [WORK_SERVER, COMPUTER_SERVER];
/// `{"disabled": [names]}` under the data dir: built-in plugins the user switched off.
const BUILTINS_FILE: &str = "plugins/builtins.json";

#[must_use]
pub fn is_builtin(name: &str) -> bool {
    BUILTIN_SERVERS.contains(&name)
}

/// Built-in servers switched off by the user (missing or unreadable file = none).
pub async fn disabled_builtins(data_dir: &Path) -> Vec<String> {
    let Ok(bytes) = tokio::fs::read(data_dir.join(BUILTINS_FILE)).await else {
        return Vec::new();
    };
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|v| v.get("disabled").cloned())
        .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|n| SWITCHABLE_BUILTINS.contains(&n.as_str()))
        .collect()
}

/// Switches a built-in server on or off for new sessions.
///
/// # Errors
///
/// Not a switchable built-in, I/O.
pub async fn set_builtin_enabled(data_dir: &Path, name: &str, on: bool) -> Result<()> {
    if !SWITCHABLE_BUILTINS.contains(&name) {
        return Err(crate::ConfigError::invalid(format!(
            "{name} cannot be switched off"
        )));
    }
    let mut disabled = disabled_builtins(data_dir).await;
    disabled.retain(|n| n != name);
    if !on {
        disabled.push(name.to_owned());
    }
    let path = data_dir.join(BUILTINS_FILE);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let text = format!("{}\n", json!({ "disabled": disabled }));
    harness_settings::fsx::write_atomic(&path, text.as_bytes()).await?;
    Ok(())
}

/// Transports the agent advertised in `agentCapabilities.mcpCapabilities` (stdio is always supported).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AcpMcpCapabilities {
    pub http: bool,
    pub sse: bool,
}

/// Enabled overlay servers for `agent_id`, in ACP `McpServer` shape. Servers the agent cannot take (http/sse not
/// advertised) are skipped.
///
/// # Errors
///
/// Unreadable or invalid `mcp.json` (the hub logs it and continues without the overlay).
pub async fn acp_servers(
    data_dir: &Path,
    agent_id: &str,
    caps: AcpMcpCapabilities,
) -> Result<Vec<Value>> {
    Ok(servers(data_dir, agent_id)
        .await?
        .iter()
        .filter_map(|s| to_acp(s, caps))
        .collect())
}

/// Enabled overlay servers for `agent_id` (built-in names skipped), as parsed entries.
///
/// # Errors
///
/// Unreadable or invalid `mcp.json`.
pub async fn servers(data_dir: &Path, agent_id: &str) -> Result<Vec<McpServer>> {
    let servers = list_json(&data_dir.join(OVERLAY_FILE), Dialect::Overlay).await?;
    Ok(servers
        .into_iter()
        .filter(|s| s.enabled != Some(false) && !is_builtin(&s.name))
        .filter(|s| s.agents.is_empty() || s.agents.iter().any(|a| a == agent_id))
        .collect())
}

fn pairs(map: &std::collections::BTreeMap<String, String>) -> Value {
    Value::Array(
        map.iter()
            .map(|(k, v)| json!({ "name": k, "value": v }))
            .collect(),
    )
}

/// ACP shape of one server, or `None` when the transport is not supported by the agent.
#[must_use]
pub fn to_acp(server: &McpServer, caps: AcpMcpCapabilities) -> Option<Value> {
    match server.transport {
        McpTransport::Stdio => Some(json!({
            "name": server.name,
            "command": server.command.clone().unwrap_or_default(),
            "args": server.args,
            "env": pairs(&server.env),
        })),
        McpTransport::Http if caps.http => Some(json!({
            "type": "http", "name": server.name, "url": server.url, "headers": pairs(&server.headers),
        })),
        McpTransport::Sse if caps.sse => Some(json!({
            "type": "sse", "name": server.name, "url": server.url, "headers": pairs(&server.headers),
        })),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn filters_and_converts() -> Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join(OVERLAY_FILE),
            r#"{"mcpServers":{
                "a":{"command":"a.exe","args":["x"],"env":{"K":"V"}},
                "b":{"type":"http","url":"http://h"},
                "c":{"command":"c","enabled":false},
                "d":{"command":"d","agents":["codex"]},
                "harness-memory":{"command":"evil"},
                "harness-computer":{"command":"evil"}}}"#,
        )?;
        let got = acp_servers(dir.path(), "claude-code", AcpMcpCapabilities::default()).await?;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["env"][0]["name"], "K");
        let got = acp_servers(
            dir.path(),
            "codex",
            AcpMcpCapabilities {
                http: true,
                sse: false,
            },
        )
        .await?;
        let names: Vec<&str> = got.iter().filter_map(|v| v["name"].as_str()).collect();
        assert_eq!(names, ["a", "b", "d"]);
        let none = acp_servers(
            &dir.path().join("none"),
            "codex",
            AcpMcpCapabilities::default(),
        )
        .await?;
        assert_eq!(none, Vec::<Value>::new());
        Ok(())
    }
}
