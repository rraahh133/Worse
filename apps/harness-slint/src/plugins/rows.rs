//! Projects the plugin store state into `PluginsUi`.

use harness_agent_config::{AgentKind, Concept};
use harness_plugins::{InstalledPlugin, Listing, Plugin, Source};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use super::{with, State, BUILTIN, BUILTINS};
use crate::ui::{MarketRow, PluginPart, PluginRow, PluginsUi};

fn sv(s: &str) -> SharedString {
    SharedString::from(s)
}

fn agent_name(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::ClaudeCode => "Claude Code",
        AgentKind::Codex => "Codex",
        AgentKind::Agy => "agy",
        AgentKind::Harness => "Harness skills",
    }
}

/// "Claude Code, Codex, Harness skills, all agents (MCP)".
fn reach(record: &InstalledPlugin) -> String {
    let mut out: Vec<&str> = Vec::new();
    for w in &record.written {
        let name = if w.concept == Concept::Mcp {
            "all agents (MCP)"
        } else {
            agent_name(w.agent)
        };
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out.join(", ")
}

/// Whether anything installable is declared, judged from the entry alone (the detail view reads the files).
fn installable(listing: &Listing) -> (bool, String) {
    if let Source::Unsupported(why) = &listing.source {
        return (false, why.clone());
    }
    let keys: Vec<&str> = listing.components.keys().map(String::as_str).collect();
    let only_lsp = !keys.is_empty() && keys.iter().all(|k| *k == "lspServers" || *k == "hooks");
    if only_lsp && !listing.strict {
        return (
            false,
            "Language servers only: they work in Claude Code's own plugin system".to_owned(),
        );
    }
    (true, String::new())
}

/// Logo of a homepage: (image, loaded).
pub(super) fn logo(s: &State, homepage: Option<&str>) -> (slint::Image, bool) {
    homepage
        .filter(|h| !h.is_empty())
        .and_then(|h| super::icons::get(&s.data_dir, h))
        .map_or_else(|| (slint::Image::default(), false), |i| (i, true))
}

fn listing_row(s: &State, market: &str, official: bool, l: &Listing) -> PluginRow {
    let key = format!("{market}/{}", l.name);
    let record = s
        .installed
        .plugins
        .get(&l.name)
        .filter(|r| r.marketplace == market);
    let (ok, note) = installable(l);
    let pinned = match &l.source {
        Source::Git { rev: Some(rev), .. } if rev.len() == 40 => Some(rev.as_str()),
        _ => None,
    };
    let update = record.is_some_and(|r| {
        pinned.is_some_and(|p| r.rev.as_deref() != Some(p))
            || (l.version.is_some() && r.version != l.version)
    });
    let (icon, has_icon) = logo(s, l.homepage.as_deref());
    PluginRow {
        icon,
        has_icon,
        verified: official,
        busy: s.busy.as_deref() == Some(key.as_str()),
        key: sv(&key),
        name: sv(&l.name),
        title: sv(l.title()),
        description: sv(&l.description),
        category: sv(l.category.as_deref().unwrap_or("")),
        author: sv(l.author.as_deref().unwrap_or("")),
        marketplace: sv(market),
        version: sv(l.version.as_deref().unwrap_or("")),
        installed: record.is_some(),
        enabled: record.is_some_and(|r| r.enabled),
        update,
        installable: ok,
        builtin: false,
        reach: sv(&record.map(reach).unwrap_or_default()),
        note: sv(&note),
    }
}

/// A record whose marketplace no longer lists it (still shown under Installed).
fn orphan_row(s: &State, name: &str, r: &InstalledPlugin) -> PluginRow {
    let key = format!("{}/{name}", r.marketplace);
    PluginRow {
        busy: s.busy.as_deref() == Some(key.as_str()),
        key: sv(&key),
        name: sv(name),
        title: sv(name),
        marketplace: sv(&r.marketplace),
        version: sv(r.version.as_deref().unwrap_or("")),
        installed: true,
        enabled: r.enabled,
        installable: true,
        reach: sv(&reach(r)),
        note: sv("No longer listed in its marketplace"),
        ..PluginRow::default()
    }
}

fn builtin_rows(s: &State) -> Vec<PluginRow> {
    BUILTINS
        .iter()
        .map(|(server, title, description)| {
            let key = format!("{BUILTIN}/{server}");
            let on = !s.disabled_builtins.iter().any(|d| d == server);
            PluginRow {
                busy: s.busy.as_deref() == Some(key.as_str()),
                key: sv(&key),
                name: sv(server),
                title: sv(title),
                description: sv(description),
                category: sv("built-in"),
                author: sv("Harness"),
                marketplace: sv(BUILTIN),
                installed: true,
                enabled: on,
                installable: true,
                builtin: true,
                verified: true,
                reach: sv("all agents (MCP)"),
                ..PluginRow::default()
            }
        })
        .collect()
}

fn matches(row: &PluginRow, query: &str, category: &str) -> bool {
    let q = query.trim().to_lowercase();
    (category.is_empty() || row.category.as_str() == category)
        && (q.is_empty()
            || [
                &row.name,
                &row.title,
                &row.description,
                &row.category,
                &row.author,
            ]
            .iter()
            .any(|f| f.to_lowercase().contains(&q)))
}

fn all_rows(s: &State) -> Vec<PluginRow> {
    let mut rows = builtin_rows(s);
    let mut listed: Vec<PluginRow> = Vec::new();
    for m in &s.markets {
        let Some(data) = &m.data else { continue };
        let official = m.source.source == harness_plugins::store::DEFAULT_MARKETPLACE;
        listed.extend(
            data.plugins
                .iter()
                .map(|l| listing_row(s, &data.name, official, l)),
        );
    }
    for (name, r) in &s.installed.plugins {
        if !listed
            .iter()
            .any(|row| row.name.as_str() == name && row.marketplace.as_str() == r.marketplace)
        {
            listed.push(orphan_row(s, name, r));
        }
    }
    listed.sort_by_key(|row| row.title.to_lowercase());
    rows.extend(listed);
    rows
}

fn parts(plugin: &Plugin, record: Option<&InstalledPlugin>) -> Vec<PluginPart> {
    let written = |concept: Concept, name: &str| {
        record.map_or(true, |r| {
            r.written
                .iter()
                .any(|w| w.concept == concept && w.name.ends_with(name))
        })
    };
    let mut out = Vec::new();
    let mut push = |kind: &str, name: &str, detail: &str, installed: bool| {
        out.push(PluginPart {
            kind: sv(kind),
            name: sv(name),
            detail: sv(detail),
            installed,
        });
    };
    for s in &plugin.skills {
        let d = s.skill.description.as_deref().unwrap_or("");
        push(
            "skill",
            &s.skill.name,
            d,
            written(Concept::Skill, &s.skill.name),
        );
    }
    for c in &plugin.commands {
        let d = c.description.as_deref().unwrap_or("");
        push(
            "command",
            &format!("/{}", c.name),
            d,
            written(Concept::Command, &c.name),
        );
    }
    for a in &plugin.subagents {
        let d = a.description.as_deref().unwrap_or("");
        push("subagent", &a.name, d, written(Concept::Subagent, &a.name));
    }
    for m in &plugin.mcp {
        let d = m
            .url
            .clone()
            .or_else(|| m.command.clone())
            .unwrap_or_default();
        push("mcp", &m.name, &d, written(Concept::Mcp, &m.name));
    }
    if plugin.hooks {
        push(
            "hook",
            "hooks",
            "Not installed: hooks run shell commands",
            false,
        );
    }
    if plugin.lsp {
        push(
            "lsp",
            "language servers",
            "Claude Code plugin system only",
            false,
        );
    }
    out
}

/// Pushes the whole page state to Slint.
pub(super) fn push() {
    let _ = with(|s| {
        let Some(window) = s.window.upgrade() else {
            return;
        };
        let ui = window.global::<PluginsUi>();
        super::connector_rows::push(s, &ui);
        if s.section == "connectors" {
            ui.set_section(sv(&s.section));
            ui.set_tab(sv(&s.tab));
            ui.set_loading(s.loading);
            ui.set_status(sv(&s.status));
            ui.set_status_error(s.status_error);
            return;
        }
        let all = all_rows(s);
        let detail = s
            .detail_key
            .as_ref()
            .and_then(|key| all.iter().find(|r| r.key.as_str() == key).cloned());
        let mut categories: Vec<String> = all
            .iter()
            .map(|r| r.category.to_string())
            .filter(|c| !c.is_empty())
            .collect();
        categories.sort();
        categories.dedup();
        let (yours, more): (Vec<PluginRow>, Vec<PluginRow>) = all
            .into_iter()
            .filter(|r| matches(r, &s.query, &s.category))
            .partition(|r| r.installed);
        let yours = if s.tab == "discover" {
            Vec::new()
        } else {
            yours
        };
        let more = if s.tab == "yours" { Vec::new() } else { more };
        let hidden = more.len().saturating_sub(s.limit);
        let shown: Vec<PluginRow> = more.into_iter().take(s.limit).collect();
        ui.set_yours(ModelRc::new(VecModel::from(yours)));
        let (homepage, notes, parts) = match (&detail, &s.detail) {
            (Some(row), Some(plugin)) => {
                let record = s.installed.plugins.get(row.name.as_str());
                let homepage = s
                    .markets
                    .iter()
                    .filter_map(|m| m.data.as_ref())
                    .find(|d| d.name == row.marketplace.as_str())
                    .and_then(|d| d.get(row.name.as_str()))
                    .and_then(|l| l.homepage.clone())
                    .unwrap_or_default();
                (
                    homepage,
                    plugin.notes.clone(),
                    parts(plugin, record.filter(|r| row.installed && r.enabled)),
                )
            }
            _ => (String::new(), Vec::new(), Vec::new()),
        };
        ui.set_categories(ModelRc::new(VecModel::from(
            categories.iter().map(|c| sv(c)).collect::<Vec<_>>(),
        )));
        ui.set_rows(ModelRc::new(VecModel::from(shown)));
        ui.set_hidden(i32::try_from(hidden).unwrap_or(i32::MAX));
        ui.set_loading(s.loading);
        ui.set_status(sv(&s.status));
        ui.set_status_error(s.status_error);
        ui.set_detail_open(detail.is_some());
        ui.set_detail(detail.unwrap_or_default());
        ui.set_detail_loading(s.detail_loading);
        ui.set_homepage(sv(&homepage));
        ui.set_notes(ModelRc::new(VecModel::from(
            notes.iter().map(|n| sv(n)).collect::<Vec<_>>(),
        )));
        ui.set_parts(ModelRc::new(VecModel::from(parts)));
        let markets: Vec<MarketRow> = s
            .markets
            .iter()
            .map(|m| MarketRow {
                source: sv(&m.source.source),
                name: sv(m.data.as_ref().map_or("", |d| d.name.as_str())),
                count: m
                    .data
                    .as_ref()
                    .map_or(0, |d| i32::try_from(d.plugins.len()).unwrap_or(0)),
                error: sv(&m.error),
                default: m.source.source == harness_plugins::store::DEFAULT_MARKETPLACE,
            })
            .collect();
        ui.set_markets(ModelRc::new(VecModel::from(markets)));
        ui.set_tab(sv(&s.tab));
        ui.set_section(sv(&s.section));
    });
}
