use crate::hub_chat::shared_commands as shared;

use super::{
    catchup_block, injection_block, partial_block, queue, seed_block, text_block, tool_digests,
    AgentId, ContextTurn, MemoryTarget, SessionActor, ToolContent, TurnId, TurnView, Value,
};

impl SessionActor {
    #[allow(clippy::too_many_lines)]
    pub(super) async fn prompt_blocks(
        &self,
        turn_id: &TurnId,
        agent: &AgentId,
        fresh: bool,
        previous_seq: u32,
        caps: crate::context::Caps,
    ) -> Vec<Value> {
        let Some(turn) = self.turn(turn_id) else {
            return Vec::new();
        };
        let seq = turn.seq;
        let tools = tool_digests(&turn.tool_calls);
        let mut blocks = Vec::new();
        let earlier = |include: &dyn Fn(&TurnView) -> bool| -> Vec<ContextTurn> {
            self.turns
                .iter()
                .filter(|t| t.seq < seq && !t.assistant_text.is_empty() && include(t))
                .map(|t| ContextTurn {
                    seq: t.seq,
                    agent: t
                        .agent
                        .as_ref()
                        .map_or_else(String::new, |a| self.ctx.display_name(a)),
                    user: queue::user_with_steers(t),
                    assistant: t.assistant_text.clone(),
                    tools: tool_digests(&t.tool_calls),
                })
                .collect()
        };
        let mut notes_pushed = false;
        if fresh {
            blocks.push(text_block(&self.about_block().await));
            blocks.push(text_block(&self.memory_block().await));
            if let Some(recall) = self.recall_block(&turn.user_text).await {
                blocks.push(text_block(&recall));
            }
            if let Some(graph) = self.graph_block().await {
                blocks.push(text_block(&graph));
            }
            if let Some(map) = self.map_block().await {
                blocks.push(text_block(&map));
            }
            if let Some(notes) = self.notes_block().await {
                blocks.push(text_block(&notes));
                notes_pushed = true;
            }
            if let Some(seed) = seed_block(&earlier(&|_| true)) {
                blocks.push(text_block(&seed));
            }
        } else if let Some(catchup) = catchup_block(&earlier(&|t| {
            t.seq > previous_seq && t.agent.as_ref() != Some(agent)
        })) {
            blocks.push(text_block(&catchup));
            if let Some(notes) = self.notes_block().await {
                blocks.push(text_block(&notes));
                notes_pushed = true;
            }
        }
        let mut failed_over = false;
        for attempt in turn
            .attempts
            .iter()
            .filter(|a| a.status == "quotaExhausted")
        {
            failed_over = true;
            blocks.push(text_block(&partial_block(
                &self.ctx.display_name(&attempt.agent),
                &attempt.partial_text,
                &tools,
            )));
        }
        if failed_over && !notes_pushed {
            if let Some(notes) = self.notes_block().await {
                blocks.push(text_block(&notes));
            }
        }
        blocks.extend(
            crate::context::blocks(
                &self.ctx,
                &self.row.workspace_path,
                &turn.context,
                caps,
                &self.ctx.display_name(agent),
            )
            .await,
        );
        if let Some(hint) =
            crate::agy_delegate::hint(&self.ctx, agent, &self.row.id, fresh, &turn.user_text)
        {
            if crate::orchestra::may_delegate(&self.ctx, agent).await {
                blocks.push(text_block(hint));
            }
        }
        if let Some(hint) = crate::codex_delegate::hint(&self.ctx, agent, &self.row.id).await {
            blocks.push(text_block(hint));
        }
        let mut user = None;
        if let Some((name, input)) = crate::hub_chat::skill_call(&turn.user_text) {
            if let Some(skill) = crate::hub_chat::harness_skills(&self.ctx)
                .await
                .into_iter()
                .find(|s| s.name == name)
            {
                user = Some(crate::hub_chat::skill_prompt(&skill, input));
            }
            // 修复：agy/自定义提供商没有命令文件，插件命令（装在 Claude Code 用户目录）在这里展开成文本。
            if user.is_none() && shared::needs_shared(&self.ctx, agent) {
                if let Some((command, _)) = shared::shared_commands(&self.ctx)
                    .await
                    .into_iter()
                    .find(|(c, _)| c.name == name)
                {
                    user = Some(shared::command_prompt(&command, input));
                }
            }
        }
        blocks.push(text_block(user.as_deref().unwrap_or(&turn.user_text)));
        blocks
    }

    pub(super) async fn memory_block(&self) -> String {
        let memory = self.ctx.memory.clone();
        let project = std::path::PathBuf::from(&self.row.workspace_path);
        let root = self.row.workspace_path.clone();
        tokio::task::spawn_blocking(move || {
            let global = memory.read(&MemoryTarget::Global).unwrap_or_default();
            let project = memory
                .read(&MemoryTarget::Project(project))
                .unwrap_or_default();
            injection_block(&global, &project, &root)
        })
        .await
        .unwrap_or_default()
    }

    /// What the local memory holds for this request (pinned memories, related ones, earlier sessions of the
    /// project); `None` when recall is switched off. Built once per fresh agent session, so it stays fixed
    /// for that session.
    pub(super) async fn recall_block(&self, request: &str) -> Option<String> {
        let home = self.ctx.config.home.clone();
        let project = crate::recall::project_key(&self.row.workspace_path);
        let session = self.row.id.as_str().to_owned();
        let request = request.to_owned();
        tokio::task::spawn_blocking(move || {
            let store = crate::recall::Store::new(&home);
            if !store.config().recall {
                return None;
            }
            let memories = store.load();
            Some(crate::recall::recall_block(&crate::recall::RecallInput {
                memories: &memories,
                project: Some(&project),
                session: &session,
                query: &request,
                now: crate::recall::now_ms(),
            }))
        })
        .await
        .ok()
        .flatten()
    }

    /// What Harness is and which of its tool servers this session has.
    pub(super) async fn about_block(&self) -> String {
        let child = self.ctx.subagents.is_child(&self.row.id);
        let delegate = self.ctx.subagents.endpoint.get().is_some();
        let computer = matches!(
            self.ctx.config.helpers,
            crate::helpers::HelperLaunch::SelfExe { .. }
        );
        let home = self.ctx.config.home.clone();
        let workspace = self.row.workspace_path.clone();
        let (graph, concise) = tokio::task::spawn_blocking(move || {
            let config = crate::recall::Store::new(&home).config();
            (
                config.graph && crate::graphify::mcp_server(&workspace).is_some(),
                config.concise,
            )
        })
        .await
        .unwrap_or((false, true));
        let mut text = crate::about::block(crate::about::Attached {
            delegate: delegate && !child,
            child: delegate && child,
            computer,
            graph,
        });
        if concise {
            text.push('\n');
            text.push_str(crate::about::STYLE);
        }
        text
    }

    /// The note about the project's Graphify graph; `None` without a graph or with the switch off.
    pub(super) async fn graph_block(&self) -> Option<String> {
        let home = self.ctx.config.home.clone();
        let workspace = self.row.workspace_path.clone();
        tokio::task::spawn_blocking(move || {
            if !crate::recall::Store::new(&home).config().graph {
                return None;
            }
            crate::graphify::prompt_block(&workspace, crate::recall::now_ms())
        })
        .await
        .ok()
        .flatten()
    }

    pub(super) async fn map_block(&self) -> Option<String> {
        let home = self.ctx.config.home.clone();
        let workspace = self.row.workspace_path.clone();
        tokio::task::spawn_blocking(move || {
            if !crate::recall::Store::new(&home).config().graph {
                return None;
            }
            crate::project_map::prompt_block(&home, &workspace)
        })
        .await
        .ok()
        .flatten()
    }

    /// Writes the digest of this session to the local memory, so a later session, or another agent, can pick
    /// up from it. Subagent sessions are left out: their parent's digest covers the work.
    pub(super) fn capture_episode(&self) {
        if self.ctx.subagents.get(&self.row.id).is_some() {
            return;
        }
        let turns: Vec<crate::recall::EpisodeTurn> = self
            .turns
            .iter()
            .map(|t| crate::recall::EpisodeTurn {
                user: queue::user_with_steers(t),
                assistant: t.assistant_text.clone(),
                files: t
                    .tool_calls
                    .iter()
                    .flat_map(|call| call.content.iter())
                    .filter_map(|item| match item {
                        ToolContent::Diff { path, .. } => Some(self.relative(path)),
                        _ => None,
                    })
                    .collect(),
            })
            .collect();
        let agent = self
            .turns
            .iter()
            .rev()
            .find_map(|t| t.agent.as_ref())
            .map(|a| self.ctx.display_name(a))
            .unwrap_or_default();
        let home = self.ctx.config.home.clone();
        let session = self.row.id.as_str().to_owned();
        let project = self.row.workspace_path.clone();
        let title = self.row.title.clone();
        drop(tokio::task::spawn_blocking(move || {
            let store = crate::recall::Store::new(&home);
            if !store.config().capture {
                return;
            }
            let text = crate::recall::episode_text(&turns);
            if text.is_empty() {
                return;
            }
            if let Err(error) = store.put_episode(&session, &project, &title, &agent, &text) {
                tracing::warn!(%error, "saving the session digest failed");
            }
        }));
    }

    /// `path` relative to the session's workspace when it lies inside it (shorter, and the same on every
    /// machine).
    pub(super) fn relative(&self, path: &str) -> String {
        let normal = path.replace('\\', "/");
        let root = crate::recall::project_key(&self.row.workspace_path);
        let inside = if cfg!(windows) {
            normal.to_lowercase().starts_with(&format!("{root}/"))
        } else {
            normal.starts_with(&format!("{root}/"))
        };
        if inside {
            normal[root.len() + 1..].to_owned()
        } else {
            normal
        }
    }

    pub(super) async fn notes_block(&self) -> Option<String> {
        let home = self.ctx.config.home.clone();
        let session = self.row.id.as_str().to_owned();
        tokio::task::spawn_blocking(move || crate::session_notes::block(&home, &session))
            .await
            .ok()
            .filter(|block| !block.is_empty())
    }
}
