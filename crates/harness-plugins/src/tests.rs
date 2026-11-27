use std::path::Path;
use std::sync::Arc;

use harness_agent_config::{
    AgentConfigService, AgentKind, Concept, ConfigPaths, ConfigTarget, Scope, StaticProbe,
};

use crate::install::{plan_install, plan_uninstall, server_name};
use crate::marketplace::{self, Source};
use crate::plugin;
use crate::record::InstalledPlugin;

const MARKETPLACE: &str = r#"{
  "name": "claude-plugins-official",
  "owner": {"name": "Anthropic"},
  "plugins": [
    {"name": "local-one", "description": "d", "source": "./plugins/local-one", "category": "development",
     "author": {"name": "A"}},
    {"name": "url-one", "source": {"source": "url", "url": "https://github.com/o/r.git", "sha": "0123456789abcdef0123456789abcdef01234567"}},
    {"name": "sub-one", "source": {"source": "git-subdir", "url": "https://github.com/o/r.git", "path": "skills", "ref": "main"},
     "strict": false, "skills": ["./a", "./b"]},
    {"name": "gh-one", "source": {"source": "github", "repo": "o/r"}},
    {"name": "cmd-one", "source": {"source": "command", "command": "xcrun x"}},
    {"name": "escape", "source": "../../etc"},
    {"name": "ssh", "source": {"source": "url", "url": "ssh://git@github.com/o/r.git"}},
    {"name": "badrev", "source": {"source": "url", "url": "https://github.com/o/r.git", "ref": "--upload-pack=x"}},
    {"name": "../evil", "source": "./x"}
  ]
}"#;

#[test]
fn marketplace_reads_every_supported_source_and_refuses_unsafe_ones() {
    let m = marketplace::parse(MARKETPLACE.as_bytes()).unwrap();
    assert_eq!(m.name, "claude-plugins-official");
    let names: Vec<&str> = m.plugins.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        ["local-one", "url-one", "sub-one", "gh-one", "cmd-one"]
    );
    assert_eq!(
        m.get("local-one").unwrap().source,
        Source::Relative("plugins/local-one".into())
    );
    assert_eq!(
        m.get("sub-one").unwrap().source,
        Source::Git {
            url: "https://github.com/o/r.git".into(),
            rev: Some("main".into()),
            path: Some("skills".into())
        }
    );
    assert!(!m.get("sub-one").unwrap().strict);
    assert!(m.get("sub-one").unwrap().components.contains_key("skills"));
    assert!(
        matches!(m.get("gh-one").unwrap().source, Source::Git { ref url, .. } if url == "https://github.com/o/r.git")
    );
    assert!(matches!(
        m.get("cmd-one").unwrap().source,
        Source::Unsupported(_)
    ));
    let skipped: Vec<&str> = m.skipped.iter().map(|s| s.0.as_str()).collect();
    assert_eq!(skipped, ["escape", "ssh", "badrev", "../evil"]);
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn sample_plugin(root: &Path) {
    write(
        &root.join(".claude-plugin/plugin.json"),
        r#"{"name": "demo", "version": "1.2.0", "description": "Demo"}"#,
    );
    write(
        &root.join("skills/review/SKILL.md"),
        "---\nname: review\ndescription: Reviews code\n---\nDo a review.\n",
    );
    write(&root.join("skills/review/references/guide.md"), "guide\n");
    write(
        &root.join("commands/ship.md"),
        "---\ndescription: Ship it\n---\nShip $ARGUMENTS\n",
    );
    write(
        &root.join("agents/helper.md"),
        "---\nname: helper\ndescription: Helps\n---\nYou help.\n",
    );
    write(
        &root.join(".mcp.json"),
        r#"{"mcpServers": {"demo": {"command": "node", "args": ["${CLAUDE_PLUGIN_ROOT}/server.js"]},
            "extra": {"type": "http", "url": "https://example.com/mcp"}}}"#,
    );
    write(&root.join("hooks/hooks.json"), "{}");
}

#[tokio::test]
async fn plugin_components_are_read_with_the_root_filled_in() {
    let dir = tempfile::tempdir().unwrap();
    sample_plugin(dir.path());
    let p = plugin::load(dir.path(), None).await.unwrap();
    assert_eq!(p.name, "demo");
    assert_eq!(p.version.as_deref(), Some("1.2.0"));
    assert_eq!(p.skills.len(), 1);
    assert_eq!(
        p.skills[0].files,
        [("references/guide.md".to_owned(), "guide\n".to_owned())]
    );
    assert_eq!(p.commands[0].name, "ship");
    assert_eq!(p.subagents[0].name, "helper");
    assert_eq!(p.mcp.len(), 2);
    let demo = p.mcp.iter().find(|s| s.name == "demo").unwrap();
    assert!(!demo.args[0].contains("CLAUDE_PLUGIN_ROOT"));
    assert!(demo.args[0].starts_with(&*dir.path().to_string_lossy()));
    assert!(p.hooks);
}

#[tokio::test]
async fn entry_declared_skill_folders_are_used_for_non_strict_plugins() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("a/SKILL.md"),
        "---\nname: a\ndescription: A\n---\nA\n",
    );
    write(
        &dir.path().join("b/SKILL.md"),
        "---\nname: b\ndescription: B\n---\nB\n",
    );
    write(
        &dir.path().join("c/SKILL.md"),
        "---\nname: c\ndescription: C\n---\nC\n",
    );
    let m = marketplace::parse(MARKETPLACE.as_bytes()).unwrap();
    let p = plugin::load(dir.path(), m.get("sub-one")).await.unwrap();
    let names: Vec<&str> = p.skills.iter().map(|s| s.skill.name.as_str()).collect();
    assert_eq!(names, ["a", "b"]);
}

#[tokio::test]
async fn bare_server_maps_load_and_needed_variables_are_reported() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join(".mcp.json"),
        r#"{"lin": {"type": "http", "url": "https://x/mcp", "headers": {"Authorization": "Bearer ${LIN_TOKEN}"}}}"#,
    );
    let m = marketplace::parse(MARKETPLACE.as_bytes()).unwrap();
    let p = plugin::load(dir.path(), m.get("local-one")).await.unwrap();
    assert_eq!(p.mcp.len(), 1);
    assert!(
        p.notes.iter().any(|n| n.contains("LIN_TOKEN")),
        "{:?}",
        p.notes
    );
}

/// Manual check against a real marketplace checkout:
/// `HARNESS_PLUGINS_SMOKE=<checkout> cargo test -p harness-plugins -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "needs a marketplace checkout"]
async fn every_local_plugin_of_a_real_marketplace_loads() {
    let Some(dir) = std::env::var_os("HARNESS_PLUGINS_SMOKE") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let m = crate::store::read_marketplace(&dir).await.unwrap();
    println!(
        "{} plugins, {} skipped: {:?}",
        m.plugins.len(),
        m.skipped.len(),
        m.skipped
    );
    let (mut loaded, mut empty) = (0, Vec::new());
    for listing in &m.plugins {
        let Source::Relative(path) = &listing.source else {
            continue;
        };
        let p = plugin::load(&dir.join(path), Some(listing))
            .await
            .unwrap_or_else(|e| panic!("{}: {e}", listing.name));
        loaded += 1;
        if p.is_empty() {
            empty.push((p.name.clone(), p.notes.clone()));
        }
    }
    println!("loaded {loaded} local plugins; nothing installable in {empty:?}");
}

/// Manual download check: `HARNESS_PLUGINS_SMOKE=<checkout> HARNESS_PLUGINS_FETCH=<plugin> cargo test -p
/// harness-plugins -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "needs the network"]
async fn a_git_sourced_plugin_downloads_and_loads() {
    let (Some(dir), Some(name)) = (
        std::env::var_os("HARNESS_PLUGINS_SMOKE"),
        std::env::var("HARNESS_PLUGINS_FETCH").ok(),
    ) else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let m = crate::store::read_marketplace(&dir).await.unwrap();
    let listing = m.get(&name).unwrap();
    let data = tempfile::tempdir().unwrap();
    let (root, rev) = crate::install::locate(data.path(), &dir, &m.name, listing).unwrap();
    let p = plugin::load(&root, Some(listing)).await.unwrap();
    println!(
        "{name} @ {rev:?}: {} skills, {} commands, {} agents, {} mcp, notes {:?}",
        p.skills.len(),
        p.commands.len(),
        p.subagents.len(),
        p.mcp.len(),
        p.notes
    );
    assert!(!p.is_empty());
    // A second locate of a pinned sha reuses the checkout.
    assert_eq!(
        crate::install::locate(data.path(), &dir, &m.name, listing)
            .unwrap()
            .1,
        rev
    );
}

#[tokio::test]
async fn install_reaches_every_agent_keeps_user_entries_and_uninstalls_cleanly() {
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    sample_plugin(root.path());
    // The user already has a command named "ship" in Claude Code: it must stay theirs.
    write(&home.path().join(".claude/commands/ship.md"), "mine\n");
    let svc = AgentConfigService::new(
        ConfigPaths::for_home(home.path(), data.path(), None),
        Arc::new(StaticProbe::default()),
    );
    let p = plugin::load(root.path(), None).await.unwrap();
    let install = plan_install(&svc, &p, None).await.unwrap();
    assert!(install
        .kept
        .iter()
        .any(|w| w.agent == AgentKind::ClaudeCode && w.name == "ship"));
    svc.apply(&install.plan).await.unwrap();

    assert_eq!(
        std::fs::read_to_string(home.path().join(".claude/commands/ship.md")).unwrap(),
        "mine\n"
    );
    for agent in [AgentKind::ClaudeCode, AgentKind::Harness] {
        let skills = svc
            .list(Concept::Skill, ConfigTarget::new(agent, Scope::User))
            .await
            .unwrap();
        assert!(skills.iter().any(|e| e.name == "review"), "{agent:?}");
    }
    assert!(data
        .path()
        .join("skills/review/references/guide.md")
        .is_file());
    let servers = svc
        .list(
            Concept::Mcp,
            ConfigTarget::new(AgentKind::Harness, Scope::User),
        )
        .await
        .unwrap();
    let mut names: Vec<String> = servers.iter().map(|e| e.name.clone()).collect();
    names.sort();
    assert_eq!(
        names,
        [server_name("demo", "demo"), server_name("demo", "extra")]
    );

    let record = InstalledPlugin {
        enabled: true,
        written: install.written.clone(),
        ..Default::default()
    };
    // Reinstalling the same plugin updates its own entries instead of keeping them.
    let again = plan_install(&svc, &p, Some(&record)).await.unwrap();
    assert_eq!(again.written, install.written);

    svc.apply(&plan_uninstall(&svc, &record).await.unwrap())
        .await
        .unwrap();
    let left = svc
        .list(
            Concept::Mcp,
            ConfigTarget::new(AgentKind::Harness, Scope::User),
        )
        .await
        .unwrap();
    assert_eq!(left.len(), 0);
    assert!(!data.path().join("skills/review").exists());
    assert_eq!(
        std::fs::read_to_string(home.path().join(".claude/commands/ship.md")).unwrap(),
        "mine\n"
    );
}
