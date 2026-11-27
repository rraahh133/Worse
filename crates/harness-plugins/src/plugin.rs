//! A plugin on disk and its components, read with the agent-configuration parsers.

use std::path::{Path, PathBuf};

use harness_agent_config::{plugin_read, Command, McpServer, Skill, Subagent};
use serde_json::{Map, Value};

use crate::error::{PluginError, Result};
use crate::marketplace::{check_name, relative, Listing};

/// Manifest inside a plugin folder.
pub const MANIFEST_FILE: &str = ".claude-plugin/plugin.json";
/// Placeholder plugins use in MCP commands for their own folder.
const ROOT_VAR: &str = "${CLAUDE_PLUGIN_ROOT}";
/// Extra files of one skill copied with it (more are reported, not copied).
const MAX_SKILL_FILES: usize = 64;
/// Largest extra skill file copied.
const MAX_SKILL_FILE_BYTES: u64 = 512 * 1024;

/// One skill and the extra files of its folder (`(relative path, text)`).
#[derive(Debug, Clone)]
pub struct PluginSkill {
    pub skill: Skill,
    pub files: Vec<(String, String)>,
}

/// A plugin read from its folder.
#[derive(Debug, Clone)]
pub struct Plugin {
    pub name: String,
    pub description: Option<String>,
    pub version: Option<String>,
    pub root: PathBuf,
    pub skills: Vec<PluginSkill>,
    pub commands: Vec<Command>,
    pub subagents: Vec<Subagent>,
    pub mcp: Vec<McpServer>,
    /// The plugin ships hooks (never installed).
    pub hooks: bool,
    /// The plugin declares language servers (Claude Code only; not installed by Harness).
    pub lsp: bool,
    /// What was left out and why, for the detail view.
    pub notes: Vec<String>,
}

impl Plugin {
    /// True when nothing Harness can install was found.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
            && self.commands.is_empty()
            && self.subagents.is_empty()
            && self.mcp.is_empty()
    }
}

/// Reads the plugin in `root`. `listing` adds the marketplace entry's declarations (and is the only
/// manifest for `strict: false` entries without `plugin.json`).
///
/// # Errors
///
/// Invalid manifest, no manifest for a strict entry, unsafe component path, I/O.
pub async fn load(root: &Path, listing: Option<&Listing>) -> Result<Plugin> {
    let manifest_path = root.join(MANIFEST_FILE);
    let manifest: Map<String, Value> = match tokio::fs::read(&manifest_path).await {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .ok_or_else(|| PluginError::invalid("plugin.json is not a JSON object"))?,
        // 没有 plugin.json 时仍按约定目录读取（官方市场里有插件只靠条目声明组件）。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Map::new(),
        Err(e) => return Err(e.into()),
    };
    let name = manifest
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| listing.map(|l| l.name.clone()))
        .ok_or_else(|| PluginError::invalid("plugin has no name"))?;
    check_name(&name)?;
    let declared = |key: &str| -> Vec<Value> {
        let mut out = Vec::new();
        for source in [Some(&manifest), listing.map(|l| &l.components)]
            .into_iter()
            .flatten()
        {
            if let Some(v) = source.get(key) {
                out.push(v.clone());
            }
        }
        out
    };
    let mut plugin = Plugin {
        description: manifest
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                listing
                    .map(|l| l.description.clone())
                    .filter(|d| !d.is_empty())
            }),
        version: manifest
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| listing.and_then(|l| l.version.clone())),
        name,
        root: root.to_path_buf(),
        skills: Vec::new(),
        commands: Vec::new(),
        subagents: Vec::new(),
        mcp: Vec::new(),
        hooks: !declared("hooks").is_empty() || root.join("hooks").join("hooks.json").is_file(),
        lsp: !declared("lspServers").is_empty() || root.join(".lsp.json").is_file(),
        notes: Vec::new(),
    };
    load_skills(&mut plugin, &paths(root, "skills", &declared("skills"))?).await?;
    for dir in paths(root, "commands", &declared("commands"))? {
        for (command, _) in plugin_read::commands(&dir).await? {
            push_unique(&mut plugin.commands, command, |c| c.name.clone());
        }
    }
    for dir in paths(root, "agents", &declared("agents"))? {
        for (agent, _) in plugin_read::subagents(&dir).await? {
            push_unique(&mut plugin.subagents, agent, |a| a.name.clone());
        }
    }
    let mut servers = mcp_file(&root.join(".mcp.json")).await?;
    for value in declared("mcpServers") {
        match &value {
            Value::String(path) => servers.extend(mcp_file(&root.join(relative(path)?)).await?),
            Value::Object(_) => servers.extend(mcp_servers(&value)),
            _ => {}
        }
    }
    for server in servers {
        let vars = env_refs(&server);
        if !vars.is_empty() {
            plugin.notes.push(format!(
                "MCP server {} needs {} set",
                server.name,
                vars.join(", ")
            ));
        }
        push_unique(&mut plugin.mcp, with_root(server, root), |s| s.name.clone());
    }
    if plugin.hooks {
        plugin
            .notes
            .push("hooks are not installed (they run shell commands)".to_owned());
    }
    if plugin.lsp {
        plugin
            .notes
            .push("language servers work in Claude Code's own plugin system only".to_owned());
    }
    Ok(plugin)
}

/// The default folder plus declared paths (string or list), deduplicated.
fn paths(root: &Path, default: &str, declared: &[Value]) -> Result<Vec<PathBuf>> {
    let mut out = vec![root.join(default)];
    for value in declared {
        let items: Vec<&str> = match value {
            Value::String(s) => vec![s.as_str()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        for item in items {
            let path = root.join(relative(item)?);
            if !out.contains(&path) {
                out.push(path);
            }
        }
    }
    Ok(out)
}

/// A path is either a folder of skills or one skill folder (`<path>/SKILL.md`).
async fn load_skills(plugin: &mut Plugin, dirs: &[PathBuf]) -> Result<()> {
    for dir in dirs {
        let single = dir.join("SKILL.md").is_file();
        let (scan, only) = match (single, dir.parent(), dir.file_name()) {
            (true, Some(parent), Some(name)) => (
                parent.to_path_buf(),
                Some(name.to_string_lossy().into_owned()),
            ),
            _ => (dir.clone(), None),
        };
        for (skill, path) in plugin_read::skills(&scan).await? {
            if only.as_ref().is_some_and(|o| *o != skill.name)
                || plugin.skills.iter().any(|s| s.skill.name == skill.name)
            {
                continue;
            }
            let folder = path.parent().map(Path::to_path_buf).unwrap_or_default();
            let files = skill_files(&folder, &mut plugin.notes, &skill.name);
            plugin.skills.push(PluginSkill { skill, files });
        }
    }
    Ok(())
}

/// Text files of a skill folder except `SKILL.md`, as `(a/b.md, text)`.
fn skill_files(folder: &Path, notes: &mut Vec<String>, skill: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut left_out = 0usize;
    let mut stack = vec![(folder.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || (prefix.is_empty() && name == "SKILL.md") {
                continue;
            }
            let rel = format!("{prefix}{name}");
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push((entry.path(), format!("{rel}/")));
                continue;
            }
            let small = entry
                .metadata()
                .is_ok_and(|m| m.len() <= MAX_SKILL_FILE_BYTES);
            match std::fs::read_to_string(entry.path()) {
                Ok(text) if small && out.len() < MAX_SKILL_FILES => out.push((rel, text)),
                _ => left_out += 1,
            }
        }
    }
    if left_out > 0 {
        notes.push(format!(
            "skill {skill}: {left_out} binary, large or extra files are not copied"
        ));
    }
    out
}

/// Servers of a plugin MCP file; missing file = none.
async fn mcp_file(path: &Path) -> Result<Vec<McpServer>> {
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|e| PluginError::invalid(format!("{}: {e}", path.display())))?;
            Ok(mcp_servers(&value))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

/// Plugins write both `{"mcpServers": {...}}` and the bare server map.
fn mcp_servers(value: &Value) -> Vec<McpServer> {
    plugin_read::mcp_value(value.get("mcpServers").unwrap_or(value))
}

/// `${VAR}` references other than the plugin root (the user must provide them).
fn env_refs(server: &McpServer) -> Vec<String> {
    let mut texts: Vec<&str> = server.args.iter().map(String::as_str).collect();
    texts.extend(server.command.as_deref());
    texts.extend(server.url.as_deref());
    texts.extend(server.env.values().map(String::as_str));
    texts.extend(server.headers.values().map(String::as_str));
    let mut out: Vec<String> = Vec::new();
    for text in texts {
        let mut rest = text;
        while let Some(start) = rest.find("${") {
            let after = &rest[start + 2..];
            let Some(end) = after.find('}') else { break };
            let name = after[..end]
                .split(":-")
                .next()
                .unwrap_or_default()
                .to_owned();
            if format!("${{{name}}}") != ROOT_VAR && !name.is_empty() && !out.contains(&name) {
                out.push(name);
            }
            rest = &after[end..];
        }
    }
    out
}

/// `${CLAUDE_PLUGIN_ROOT}` → the plugin folder in command, args, env and url.
fn with_root(mut server: McpServer, root: &Path) -> McpServer {
    let root = root.to_string_lossy();
    let fix = |s: &mut String| {
        if s.contains(ROOT_VAR) {
            *s = s.replace(ROOT_VAR, &root);
        }
    };
    if let Some(c) = server.command.as_mut() {
        fix(c);
    }
    server.args.iter_mut().for_each(fix);
    server.env.values_mut().for_each(fix);
    if let Some(u) = server.url.as_mut() {
        fix(u);
    }
    server
}

fn push_unique<T>(items: &mut Vec<T>, item: T, key: impl Fn(&T) -> String) {
    let k = key(&item);
    if !items.iter().any(|i| key(i) == k) {
        items.push(item);
    }
}
