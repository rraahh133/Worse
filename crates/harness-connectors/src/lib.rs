//! Connectors (`docs/plans/connectors.md`): remote MCP servers the user signs in to once, then every agent uses.
//!
//! - [`catalog`]: the built-in list of official remote servers.
//! - [`state`]: `<HARNESS_HOME>/connectors/connected.json` (what is connected) and `tokens.json` (OAuth tokens;
//!   never logged or shown).
//! - [`oauth`]: MCP authorization: protected-resource and authorization-server discovery, dynamic client
//!   registration, PKCE login through a loopback redirect, refresh.
//! - [`bridge`]: the stdio MCP server (`<exe> --connector-mcp <id>`) that forwards to the remote server with the
//!   token, so agents that only speak stdio (agy, API agents) get connectors too and no token reaches an agent.
//! - [`servers`]: the connected connectors as stdio MCP server entries.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod bridge;
pub mod builtin;
pub mod catalog;
mod error;
mod http;
pub mod oauth;
pub mod state;

use std::path::{Path, PathBuf};

pub use catalog::{Auth, Client, Entry};
pub use error::{ConnectorError, Result};
pub use state::{Connected, Connection, Token, Tokens};

/// Mode flag of the bridge helper (`<exe> --connector-mcp <id>`).
pub const FLAG_CONNECTOR_MCP: &str = "--connector-mcp";

/// `<HARNESS_HOME>/connectors`.
#[must_use]
pub fn dir(home: &Path) -> PathBuf {
    home.join("connectors")
}

/// The enabled connections as stdio MCP servers run by `program` (the Harness executable) with `args` (its
/// leading mode arguments, normally just [`FLAG_CONNECTOR_MCP`]); the connector id follows.
pub async fn servers(
    home: &Path,
    program: &Path,
    args: &[String],
) -> Vec<harness_agent_config::McpServer> {
    let connected = Connected::load(home).await;
    connected
        .connectors
        .iter()
        .filter(|(_, c)| c.enabled)
        .map(|(id, _)| {
            let mut args = args.to_vec();
            args.push(id.clone());
            harness_agent_config::McpServer {
                name: id.clone(),
                transport: harness_agent_config::McpTransport::Stdio,
                command: Some(program.to_string_lossy().into_owned()),
                args,
                env: [(
                    "HARNESS_HOME".to_owned(),
                    home.to_string_lossy().into_owned(),
                )]
                .into_iter()
                .collect(),
                enabled: Some(true),
                ..harness_agent_config::McpServer::default()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
