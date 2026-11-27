//! Plugin store writes (`docs/plans/plugins.md`): plans through `harness-plugins`, and the installed record,
//! which only this worker writes (after the user confirmed the diff).

use std::path::Path;

use harness_agent_config::AgentConfigService;
use harness_plugins::{install, Installed, InstalledPlugin};

use super::reqs::PluginCommit;
use super::{plan_paths, Pending, PlanOp};

fn text(error: &harness_plugins::PluginError) -> String {
    error.to_string()
}

pub(super) async fn plan(
    svc: &AgentConfigService,
    data_dir: &Path,
    op: PlanOp,
) -> Result<(Pending, String, String, bool), String> {
    let installed = Installed::load(data_dir).await;
    let (plan, commit) = match op {
        PlanOp::InstallPlugin {
            plugin,
            marketplace,
            rev,
        } => {
            let previous = installed.plugins.get(&plugin.name);
            // 关闭状态下 written 仍保留旧记录：重新开启时据此更新而不是当成用户自己的条目。
            let planned = install::plan_install(svc, &plugin, previous)
                .await
                .map_err(|e| text(&e))?;
            if planned.written.is_empty() {
                return Err(if planned.kept.is_empty() {
                    format!("{} has nothing Harness can install", plugin.name)
                } else {
                    format!(
                        "{}: you already have entries with the same names",
                        plugin.name
                    )
                });
            }
            let record = InstalledPlugin {
                marketplace,
                version: plugin.version.clone(),
                rev,
                root: plugin.root.clone(),
                enabled: true,
                written: planned.written,
            };
            let commit = PluginCommit::Install {
                name: plugin.name.clone(),
                record,
            };
            (planned.plan, commit)
        }
        PlanOp::RemovePlugin { name, keep } => {
            let record = installed
                .plugins
                .get(&name)
                .ok_or_else(|| format!("{name} is not installed"))?;
            let plan = install::plan_uninstall(svc, record)
                .await
                .map_err(|e| text(&e))?;
            (plan, PluginCommit::Remove { name, keep })
        }
        _ => return Err("not a plugin operation".to_owned()),
    };
    let path = plan_paths(&plan);
    let diff = plan.diff();
    let no_op = plan.is_no_op();
    Ok((Pending::Plugin(plan, commit), diff, path, no_op))
}

/// Tells the Plugins page a write ended; `Err("")` = cancelled.
pub(super) fn notify(result: Result<(), String>) {
    let _ = slint::invoke_from_event_loop(move || crate::plugins::finished(result));
}

/// Applies a confirmed plugin plan, then records it.
pub(super) async fn confirm(
    svc: &AgentConfigService,
    data_dir: &Path,
    plan: &harness_agent_config::WritePlan,
    commit: PluginCommit,
) -> Result<(), String> {
    let mut result = svc
        .apply(plan)
        .await
        .map(|_| ())
        .map_err(|e| super::message(&e));
    if result.is_ok() {
        result = self::commit(data_dir, commit).await;
    }
    notify(result.clone());
    result
}

/// Switches a built-in tool server (`harness-work`, `harness-computer`) on or off.
pub(super) async fn builtin(data_dir: &Path, name: &str, on: bool) {
    let result = harness_agent_config::overlay::set_builtin_enabled(data_dir, name, on)
        .await
        .map_err(|e| super::message(&e));
    notify(result);
}

/// Records what was applied.
pub(super) async fn commit(data_dir: &Path, commit: PluginCommit) -> Result<(), String> {
    let mut installed = Installed::load(data_dir).await;
    match commit {
        PluginCommit::Install { name, record } => {
            installed.plugins.insert(name, record);
        }
        PluginCommit::Remove { name, keep: true } => {
            if let Some(record) = installed.plugins.get_mut(&name) {
                record.enabled = false;
            }
        }
        PluginCommit::Remove { name, keep: false } => {
            installed.plugins.remove(&name);
        }
    }
    installed.save(data_dir).await.map_err(|e| text(&e))
}
