//! Plugin support for `harness-plugins` (`docs/plans/plugins.md` §1): parsing a plugin's component folders,
//! and one batched overlay plan for its MCP servers.
//!
//! Uses the same parsers as the agent configuration pages, so a plugin skill, command or subagent becomes
//! exactly the [`Skill`] / [`Command`] / [`Subagent`] those pages would write.

use std::path::{Path, PathBuf};

use crate::error::{ConfigError, Result};
use crate::types::{Command, McpServer, Skill, Subagent};
use crate::{mcp, md_store, ConfigPaths, WritePlan};

/// Skills under `dir` (`<dir>/<name>/SKILL.md`); missing folder = none.
pub async fn skills(dir: &Path) -> Result<Vec<(Skill, PathBuf)>> {
    md_store::list_skills(dir).await
}

/// Commands under `dir` (`<dir>/**/<name>.md`, nested folders become `a:b`); missing folder = none.
pub async fn commands(dir: &Path) -> Result<Vec<(Command, PathBuf)>> {
    md_store::list_commands(dir, true).await
}

/// Subagents under `dir` (`<dir>/<name>.md`); missing folder = none.
pub async fn subagents(dir: &Path) -> Result<Vec<(Subagent, PathBuf)>> {
    md_store::list_subagents(dir).await
}

/// MCP servers of a server map (`name → {command|url, …}`, Claude Code keys).
#[must_use]
pub fn mcp_value(servers: &serde_json::Value) -> Vec<McpServer> {
    mcp::from_value(servers)
}

/// One write plan for the Harness overlay (`<data dir>/mcp.json`) that adds or updates `upserts` and removes
/// `removes`. Reserved built-in names are refused.
///
/// # Errors
///
/// Reserved or invalid server, unreadable overlay.
pub async fn plan_overlay(
    paths: &ConfigPaths,
    upserts: &[McpServer],
    removes: &[String],
) -> Result<WritePlan> {
    if let Some(s) = upserts.iter().find(|s| crate::overlay::is_builtin(&s.name)) {
        return Err(ConfigError::invalid(format!("{} is reserved", s.name)));
    }
    let path = paths.harness_dir.join(crate::overlay::OVERLAY_FILE);
    let change = mcp::plan_batch_json(&path, mcp::Dialect::Overlay, upserts, removes).await?;
    Ok(WritePlan {
        changes: vec![change],
    })
}
