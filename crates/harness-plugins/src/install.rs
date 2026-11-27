//! Install, disable and uninstall as write plans, one per user action, for every agent that can take each
//! component (`docs/plans/plugins.md` §1 rules 2–3a).
//!
//! Skills go to Claude Code, Codex and the Harness overlay folder (served to the other agents); commands to
//! Claude Code and Codex; subagents to Claude Code; MCP servers to the Harness overlay, which every agent
//! receives. An entry the user already has under the same name is kept, not overwritten, unless this
//! plugin wrote it before.

use std::path::{Path, PathBuf};

use harness_agent_config::{
    plugin_read, skill_catalog, AgentConfigService, AgentKind, Concept, ConfigError, ConfigTarget,
    EntryBody, McpServer, Scope, WritePlan,
};
use serde_json::Value;

use crate::error::{PluginError, Result};
use crate::marketplace::{Listing, Source};
use crate::plugin::Plugin;
use crate::record::{InstalledPlugin, Written};
use crate::{fetch, store};

/// Agents that may receive file components, in order.
const FILE_AGENTS: [AgentKind; 3] = [AgentKind::ClaudeCode, AgentKind::Codex, AgentKind::Harness];
/// Overlay key that marks a server as written by a plugin.
pub const PLUGIN_KEY: &str = "plugin";

/// The folder holding `listing`'s files and the commit they came from, downloading when needed.
///
/// Blocking (`git`); run it from `spawn_blocking`.
///
/// # Errors
///
/// Unsupported source, unsafe path, download failure.
pub fn locate(
    data_dir: &Path,
    marketplace_dir: &Path,
    marketplace: &str,
    listing: &Listing,
) -> Result<(PathBuf, Option<String>)> {
    match &listing.source {
        Source::Relative(path) => {
            let root = marketplace_dir.join(path);
            if !root.is_dir() {
                return Err(PluginError::invalid(format!(
                    "{path} is missing in the marketplace"
                )));
            }
            Ok((root, fetch::head(marketplace_dir)))
        }
        Source::Git { url, rev, path } => {
            let dir = store::cache_dir(data_dir, marketplace, &listing.name)?;
            let sha = fetch::checkout(url, rev.as_deref(), &dir)?;
            let root = path.as_ref().map_or_else(|| dir.clone(), |p| dir.join(p));
            if !root.is_dir() {
                return Err(PluginError::invalid(
                    "the plugin folder is missing in its repository",
                ));
            }
            Ok((root, Some(sha)))
        }
        Source::Unsupported(why) => Err(PluginError::invalid(why.clone())),
    }
}

/// Overlay name of a plugin server: the server's own name when it equals the plugin's, else `plugin-server`.
#[must_use]
pub fn server_name(plugin: &str, server: &str) -> String {
    if plugin == server {
        server.to_owned()
    } else {
        format!("{plugin}-{server}")
    }
}

fn supports(svc: &AgentConfigService, agent: AgentKind, concept: Concept) -> bool {
    svc.capabilities(agent)
        .iter()
        .any(|s| s.concept == concept && s.user)
}

async fn existing(svc: &AgentConfigService, concept: Concept, agent: AgentKind) -> Vec<String> {
    svc.list(concept, ConfigTarget::new(agent, Scope::User))
        .await
        .map(|entries| entries.into_iter().map(|e| e.name).collect())
        .unwrap_or_default()
}

/// One file component to write: concept, name, entry and extra skill files.
type Body = (Concept, String, EntryBody, Vec<(String, String)>);

/// What an install would write: the plan, the entries it writes, and entries skipped because the user
/// already has their own under that name.
#[derive(Debug, Default)]
pub struct InstallPlan {
    pub plan: WritePlan,
    pub written: Vec<Written>,
    pub kept: Vec<Written>,
}

/// Plans installing (or updating) `plugin`. `previous` is its installed record, if any: entries it wrote
/// before are updated, and entries it no longer has are removed.
///
/// # Errors
///
/// Planning a write failed (invalid entry, unreadable agent file).
pub async fn plan_install(
    svc: &AgentConfigService,
    plugin: &Plugin,
    previous: Option<&InstalledPlugin>,
) -> Result<InstallPlan> {
    let mine = |w: &Written| previous.is_some_and(|p| p.written.contains(w));
    let mut out = InstallPlan::default();
    let mut bodies: Vec<Body> = Vec::new();
    for s in &plugin.skills {
        bodies.push((
            Concept::Skill,
            s.skill.name.clone(),
            EntryBody::Skill(s.skill.clone()),
            s.files.clone(),
        ));
    }
    for c in &plugin.commands {
        bodies.push((
            Concept::Command,
            c.name.clone(),
            EntryBody::Command(c.clone()),
            Vec::new(),
        ));
    }
    for a in &plugin.subagents {
        bodies.push((
            Concept::Subagent,
            a.name.clone(),
            EntryBody::Subagent(a.clone()),
            Vec::new(),
        ));
    }
    for agent in FILE_AGENTS {
        for concept in [Concept::Skill, Concept::Command, Concept::Subagent] {
            if !supports(svc, agent, concept) || !bodies.iter().any(|b| b.0 == concept) {
                continue;
            }
            let taken = existing(svc, concept, agent).await;
            for (_, name, body, files) in bodies.iter().filter(|b| b.0 == concept) {
                let w = Written {
                    agent,
                    concept,
                    name: name.clone(),
                };
                if taken.contains(name) && !mine(&w) {
                    out.kept.push(w);
                    continue;
                }
                let target = ConfigTarget::new(agent, Scope::User);
                let mut plan = svc.plan_upsert(target, body, None).await?;
                if !files.is_empty() {
                    plan = skill_catalog::with_extra_files(plan, files).await?;
                }
                out.plan.changes.extend(plan.changes);
                out.written.push(w);
            }
        }
    }
    let taken = existing(svc, Concept::Mcp, AgentKind::Harness).await;
    let mut servers: Vec<McpServer> = Vec::new();
    for server in &plugin.mcp {
        let mut server = server.clone();
        server.name = server_name(&plugin.name, &server.name);
        let w = Written {
            agent: AgentKind::Harness,
            concept: Concept::Mcp,
            name: server.name.clone(),
        };
        if taken.contains(&server.name) && !mine(&w) {
            out.kept.push(w);
            continue;
        }
        server
            .extra
            .insert(PLUGIN_KEY.to_owned(), Value::String(plugin.name.clone()));
        servers.push(server);
        out.written.push(w);
    }
    let gone: Vec<String> = previous
        .map(|p| {
            p.written
                .iter()
                .filter(|w| w.concept == Concept::Mcp && !out.written.contains(w))
                .map(|w| w.name.clone())
                .collect()
        })
        .unwrap_or_default();
    if !servers.is_empty() || !gone.is_empty() {
        let plan = plugin_read::plan_overlay(svc.paths(), &servers, &gone).await?;
        out.plan.changes.extend(plan.changes);
    }
    if let Some(previous) = previous {
        let stale: Vec<Written> = previous
            .written
            .iter()
            .filter(|w| w.concept != Concept::Mcp && !out.written.contains(w))
            .cloned()
            .collect();
        out.plan
            .changes
            .extend(plan_remove(svc, &stale).await?.changes);
    }
    Ok(out)
}

/// Plans removing what a plugin wrote (uninstall, or switching it off). Entries already gone are skipped.
///
/// # Errors
///
/// Planning a delete failed for another reason than the entry being gone.
pub async fn plan_uninstall(
    svc: &AgentConfigService,
    installed: &InstalledPlugin,
) -> Result<WritePlan> {
    plan_remove(svc, &installed.written).await
}

async fn plan_remove(svc: &AgentConfigService, written: &[Written]) -> Result<WritePlan> {
    let mut plan = WritePlan::default();
    let servers: Vec<String> = written
        .iter()
        .filter(|w| w.concept == Concept::Mcp)
        .map(|w| w.name.clone())
        .collect();
    for w in written.iter().filter(|w| w.concept != Concept::Mcp) {
        let target = ConfigTarget::new(w.agent, Scope::User);
        match svc.plan_delete(w.concept, target, &w.name).await {
            Ok(p) => plan.changes.extend(p.changes),
            Err(ConfigError::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
        }
    }
    if !servers.is_empty() {
        let p = plugin_read::plan_overlay(svc.paths(), &[], &servers).await?;
        plan.changes.extend(p.changes);
    }
    Ok(plan)
}
