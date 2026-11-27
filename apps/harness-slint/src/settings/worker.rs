//! Settings worker (spec phase-9-parity.md §6.3): runs on the `harness-core` tokio runtime, owns the
//! `AgentConfigService` and the write plan awaiting confirmation, and talks to the shared `SettingsStore`.
//! Requests are served one at a time; results go back to the UI thread through `invoke_from_event_loop`.
//!
//! The UI never writes a file itself: every agent-config / document write is planned here, shown as a diff, and
//! applied only after `Req::Confirm` (with the plan's conflict guard).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use harness_agent_config::{
    launch, skill_catalog, AgentConfigService, AgentKind, AuthState, Concept, ConceptSupport,
    ConfigError, ConfigPaths, ConfigTarget, EntryBody, FileExistenceProbe, McpImport, Scope,
    WritePlan,
};
use harness_core_api as api;
use harness_settings::{relocation, Settings, SettingsStore};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::{values, State};
use crate::core::{Command, Reply, SettingsCall};

mod docs;
mod helpers;
mod plugins;
mod reqs;

use helpers::{import_summary, market_supports, message, plan_paths, post};
use reqs::Pending;
pub use reqs::{DocKind, PlanOp, Req};

/// Archive page size (hub maximum is 200).
const ARCHIVE_PAGE: u32 = 50;

/// Agents the skill marketplace installs to (user scope only, spec skill-marketplace.md §1).
const MARKET_AGENTS: [AgentKind; 3] = [AgentKind::ClaudeCode, AgentKind::Codex, AgentKind::Harness];

pub struct Worker {
    pub store: Option<Arc<SettingsStore>>,
    /// Core command channel: MEMORY.md, projects and archive go through the hub (its single writer), never a
    /// second file writer here.
    pub core: mpsc::UnboundedSender<Command>,
    pub base: PathBuf,
    pub data_dir: PathBuf,
    pub workspace: PathBuf,
}

impl Worker {
    // One flat match over the request kinds; each arm is short.
    #[allow(clippy::too_many_lines)]
    pub async fn serve(self, mut rx: mpsc::UnboundedReceiver<Req>) {
        let paths = match ConfigPaths::from_env(&self.data_dir, Some(self.workspace.clone())) {
            Ok(paths) => paths,
            Err(_) => {
                ConfigPaths::for_home(&self.data_dir, &self.data_dir, Some(self.workspace.clone()))
            }
        };
        let svc = AgentConfigService::new(paths.clone(), Arc::new(FileExistenceProbe));
        let mut pending: Option<Pending> = None;
        let mut watch = self.store.as_ref().map(|s| s.subscribe());
        if let Some(target) = relocation::pending(&self.base).await {
            let target = target.display().to_string();
            post(move |s| s.set_pending_dir(&target));
        }
        loop {
            let req = if let Some(rx_watch) = watch.as_mut() {
                tokio::select! {
                    req = rx.recv() => req,
                    changed = rx_watch.changed() => {
                        if changed.is_err() {
                            watch = None;
                            continue;
                        }
                        let snapshot = rx_watch.borrow_and_update().clone();
                        post(move |s| s.apply_snapshot(snapshot));
                        continue;
                    }
                }
            } else {
                rx.recv().await
            };
            let Some(req) = req else {
                return;
            };
            match req {
                Req::Patch { path, value } => self.patch(&path, value).await,
                Req::Shortcuts(map) => {
                    self.save(move |s| s.shortcuts = map, "saveFailed").await;
                }
                Req::Relocate(target) => {
                    let result = relocation::schedule(&self.base, &target).await;
                    let shown = target.display().to_string();
                    post(move |s| match result {
                        Ok(()) => s.set_pending_dir(&shown),
                        Err(error) => s.status(&error.to_string(), true),
                    });
                }
                Req::CancelRelocation => {
                    let result = relocation::cancel_pending(&self.base).await;
                    post(move |s| match result {
                        Ok(()) => s.set_pending_dir(""),
                        Err(error) => s.status(&error.to_string(), true),
                    });
                }
                Req::List {
                    seq,
                    concept,
                    target,
                } => {
                    let support = svc
                        .capabilities(target.agent)
                        .into_iter()
                        .find(|c| c.concept == concept);
                    let result = svc.list(concept, target).await.map_err(|e| message(&e));
                    post(move |s| s.config_listed(seq, support, result));
                }
                Req::SkillsInstalled { seq } => {
                    let mut installed = Vec::new();
                    for agent in MARKET_AGENTS {
                        let target = ConfigTarget::new(agent, Scope::User);
                        let names = svc.list(Concept::Skill, target).await.map_or_else(
                            |_| Vec::new(),
                            |entries| entries.into_iter().map(|e| e.name).collect(),
                        );
                        installed.push((agent.agent_id().to_owned(), names));
                    }
                    crate::skills::installed_loaded(seq, installed);
                }
                Req::SkillsHealth { seq } => {
                    let report = harness_agent_config::skill_health::report_for_scope(
                        &paths,
                        &MARKET_AGENTS,
                        Scope::User,
                    )
                    .await
                    .map_err(|e| e.to_string());
                    // 实时检查默认关闭（user 的配额）：只有设置里显式打开时才生成可复制的命令，
                    // Harness 自身从不启动 skill-reach（spec skill-health.md §Live check）。
                    let live = self.store.as_ref().and_then(|store| {
                        let flag = store.snapshot().skill_health.clone();
                        MARKET_AGENTS
                            .iter()
                            .find_map(|agent| {
                                let dir = harness_agent_config::skill_health::skills_dir(
                                    &paths,
                                    *agent,
                                    Scope::User,
                                )?;
                                harness_agent_config::skill_health::LiveCheck::build(
                                    flag.live_check,
                                    &flag.binary,
                                    *agent,
                                    &dir,
                                    None,
                                )
                            })
                            .map(|command| command.to_command_line())
                    });
                    crate::skills::health_loaded(seq, report, live);
                }
                Req::ReadRaw { seq, path } => {
                    // 凭据文件（auth.json、.credentials.json 等）一律不读，只读 list 返回的配置文件。
                    let text = if harness_settings::fsx::is_forbidden(&path) {
                        Err("refused: credentials file".to_string())
                    } else {
                        harness_settings::fsx::read_to_string_opt(&path)
                            .await
                            .map(Option::unwrap_or_default)
                            .map_err(|e| e.to_string())
                    };
                    post(move |s| s.raw_loaded(seq, text));
                }
                Req::Plan {
                    title,
                    op: PlanOp::ImportMcp(sources),
                } => {
                    pending = None;
                    match svc.plan_import_mcp(&sources).await {
                        Ok(import) => {
                            let summary = import_summary(&import);
                            match import.plan {
                                Some(plan) => {
                                    let path = plan_paths(&plan);
                                    let diff = plan.diff();
                                    let no_op = plan.is_no_op();
                                    pending = Some(Pending::Import(plan, summary));
                                    post(move |s| s.show_diff(title, &path, &diff, no_op));
                                }
                                None => post(move |s| s.status(&summary, false)),
                            }
                        }
                        Err(error) => {
                            let error = message(&error);
                            post(move |s| s.plan_failed(&error));
                        }
                    }
                }
                Req::Plan { title, op } => {
                    let result = self.plan(&svc, op).await;
                    pending = None;
                    match result {
                        Ok((next, diff, path, no_op)) => {
                            pending = Some(next);
                            post(move |s| s.show_diff(title, &path, &diff, no_op));
                        }
                        Err(error) => {
                            if title.ends_with("plugin") || title.contains("plugin ") {
                                plugins::notify(Err(error.clone()));
                            }
                            post(move |s| s.plan_failed(&error));
                        }
                    }
                }
                Req::Cancel => {
                    if matches!(pending.take(), Some(Pending::Plugin(..))) {
                        plugins::notify(Err(String::new()));
                    }
                }
                Req::PluginBuiltin { name, on } => {
                    plugins::builtin(&self.data_dir, &name, on).await;
                }
                Req::Confirm => {
                    let mut toast = None;
                    let result = match pending.take() {
                        Some(Pending::Plan(plan)) => {
                            svc.apply(&plan).await.map(|_| ()).map_err(|e| message(&e))
                        }
                        Some(Pending::Import(plan, summary)) => {
                            let result =
                                svc.apply(&plan).await.map(|_| ()).map_err(|e| message(&e));
                            if result.is_ok() {
                                toast = Some(summary);
                            }
                            result
                        }
                        // 记忆文件经 hub 写入（带基线指纹的冲突检查），不再由界面侧直接写盘。
                        Some(Pending::Memory(plan)) => {
                            self.call(|reply| SettingsCall::MemoryApply(plan, reply))
                                .await
                        }
                        Some(Pending::Plugin(plan, commit)) => {
                            plugins::confirm(&svc, &self.data_dir, &plan, commit).await
                        }
                        None => Ok(()),
                    };
                    post(move |s| {
                        s.applied(result);
                        if let Some(text) = toast {
                            s.status(&text, false);
                        }
                    });
                }
                Req::Doc { seq, kind, scope } => {
                    let loaded = self.load_doc(&svc, kind, scope).await;
                    post(move |s| s.doc_loaded(seq, loaded));
                }
                Req::Agents(ids) => {
                    let mut auth = Vec::new();
                    for id in &ids {
                        if let Some(kind) = AgentKind::from_agent_id(id) {
                            let state = match svc.auth_state(kind).await {
                                AuthState::Present => 0,
                                AuthState::Absent => 1,
                                AuthState::Unknown => 2,
                            };
                            auth.push((id.clone(), state));
                        }
                    }
                    let launch = launch::list(&self.data_dir)
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .map(|o| {
                            // 环境变量只显示名字，值可能是密钥。
                            let env = o.env.keys().map(|k| format!("{k}=…")).collect::<Vec<_>>();
                            (o.id, o.program, o.args.join(" "), env.join(" "))
                        })
                        .collect::<Vec<_>>();
                    post(move |s| s.agents_loaded(auth, launch));
                }
                Req::Projects { seq, query } => {
                    let query = Some(query).filter(|q| !q.trim().is_empty());
                    let request = api::ListProjects {
                        query,
                        removed: false,
                    };
                    let result = self
                        .call(|reply| SettingsCall::ListProjects(request, reply))
                        .await;
                    post(move |s| s.projects_listed(seq, result));
                }
                Req::RemoveProjects { keys, delete_data } => {
                    let removed = keys.clone();
                    let result = self
                        .call(|reply| SettingsCall::RemoveProjects {
                            keys,
                            delete_data,
                            reply,
                        })
                        .await;
                    post(move |s| s.projects_removed(removed, delete_data, result));
                }
                Req::RestoreProjects(keys) => {
                    let result = self
                        .call(|reply| SettingsCall::RestoreProjects(keys, reply))
                        .await;
                    post(move |s| s.projects_restored(result));
                }
                Req::Archived { seq, query, offset } => {
                    let request = api::ListArchived {
                        query: Some(query).filter(|q| !q.trim().is_empty()),
                        workspace_key: None,
                        sort: api::ArchiveSort::ArchivedNewest,
                        offset,
                        limit: Some(ARCHIVE_PAGE),
                    };
                    let result = self
                        .call(|reply| SettingsCall::ListArchived(request, reply))
                        .await;
                    post(move |s| s.archive_listed(seq, offset, result));
                }
                Req::Unarchive(id) => {
                    let selection = api::SessionSelection::Sessions(vec![api::SessionId::new(id)]);
                    let result = self
                        .call(|reply| SettingsCall::Unarchive(selection, reply))
                        .await
                        .map(|_| ());
                    post(move |s| s.archive_changed(result));
                }
                Req::DeleteArchived(id) => {
                    let selection = api::SessionSelection::Sessions(vec![api::SessionId::new(id)]);
                    let result = self
                        .call(|reply| SettingsCall::DeleteArchived(selection, reply))
                        .await
                        .map(|_| ());
                    post(move |s| s.archive_changed(result));
                }
            }
        }
    }

    /// Sends one settings call to the core and waits for its reply.
    async fn call<T>(&self, make: impl FnOnce(Reply<T>) -> SettingsCall) -> Result<T, String> {
        let (tx, rx) = oneshot::channel();
        if self.core.send(Command::Settings(make(tx))).is_err() {
            return Err(api::ApiError::ShutDown.to_string());
        }
        match rx.await {
            Ok(result) => result.map_err(|e| e.to_string()),
            Err(_) => Err(api::ApiError::ShutDown.to_string()),
        }
    }

    fn memory_scope(&self, scope: Scope) -> api::MemoryScope {
        match scope {
            Scope::User => api::MemoryScope::Global,
            Scope::Workspace => api::MemoryScope::Project {
                workspace_path: self.workspace.display().to_string(),
            },
        }
    }

    async fn save(&self, edit: impl FnOnce(&mut Settings), error_code: &'static str) {
        let Some(store) = &self.store else {
            post(|s| s.status("settings store unavailable", true));
            return;
        };
        if let Err(error) = store.patch(edit).await {
            let text = format!("{error_code}: {error}");
            post(move |s| s.status(&text, true));
        }
    }

    async fn patch(&self, path: &str, value: Value) {
        let path_owned = path.to_string();
        let mut failure = None;
        self.save(
            |s| {
                if let Err(error) = values::set_path(s, &path_owned, value) {
                    failure = Some(error);
                }
            },
            "settings not saved",
        )
        .await;
        if let Some(error) = failure {
            post(move |s| s.status(&error, true));
        }
    }
}
