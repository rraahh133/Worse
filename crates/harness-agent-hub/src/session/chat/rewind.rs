use super::{
    commands_of, context_usage, files, json, plan_entries, store_chat, AgentId, ApiError,
    ApiResult, ApplyFileChanges, ChangeDirection, EventKind, FileRestoreReport, Resend, RewindCmd,
    RewindOutcome, RewindResult, SendMessage, SessionActor, TurnId, Value,
};

/// Sent by Retry when the failed turn had already done part of the work.
const CONTINUE_PROMPT: &str =
    "The connection dropped before you finished. Continue the previous task \
from where you stopped; do not redo steps that are already done.";

/// A failed or interrupted turn that already produced text or ran tools: Retry continues it instead of
/// starting over.
fn resumable(turn: &harness_core_api::TurnView) -> bool {
    use harness_core_api::TurnStatus;
    matches!(turn.status, TurnStatus::Failed | TurnStatus::Interrupted)
        && (!turn.assistant_text.trim().is_empty() || !turn.tool_calls.is_empty())
}

impl SessionActor {
    /// Truncates at `command.turn` (that turn and every later one), drops all native handles, optionally
    /// restores files and resends (phase-9-parity.md §9.2.1 turn actions).
    pub(crate) async fn on_rewind(&mut self, command: RewindCmd) -> ApiResult<RewindOutcome> {
        if self.busy() {
            return Err(self.busy_error());
        }
        let Some(index) = self.turns.iter().position(|t| t.id == command.turn) else {
            return Err(ApiError::NotFound(format!(
                "turn {} in session {}",
                command.turn, self.row.id
            )));
        };
        if matches!(command.resend, Resend::Retry { .. }) && index + 1 != self.turns.len() {
            return Err(ApiError::InvalidArgument(
                "only the last turn can be retried".into(),
            ));
        }
        let first = self.turns[index].clone();
        if let Resend::Retry { agent } = &command.resend {
            if !command.restore_files && resumable(&first) {
                // 修复：连接中断后点 Retry 会删掉这一轮并重发原提示，代理从头再做一遍；
                // 已经做了一部分时保留这一轮，发一条"继续"消息，让代理接着做。
                return self.continue_turn(first, agent.clone()).await;
            }
        }
        let resend = match command.resend {
            Resend::None => None,
            Resend::Edit {
                text,
                context,
                agent,
            } => Some((
                text,
                context.unwrap_or_else(|| first.context.clone()),
                agent,
            )),
            Resend::Retry { agent } => {
                Some((first.user_text.clone(), first.context.clone(), agent))
            }
        };
        // 先校验再截断：重发失败时不能留下已被删除的轮次（操作要么整体生效，要么不改任何东西）。
        if let Some((text, context, agent)) = &resend {
            if text.trim().is_empty() {
                return Err(ApiError::InvalidArgument("message text is empty".into()));
            }
            if let Some(agent) = agent {
                self.ctx.require_available(agent)?;
            }
            crate::context::validate(&self.ctx, &self.row.workspace_path, &self.row.id, context)
                .await?;
        }
        let removed: Vec<TurnId> = self.turns[index..].iter().map(|t| t.id.clone()).collect();
        let files = if command.restore_files {
            Some(self.restore_for_rewind(&removed).await?)
        } else {
            None
        };
        let (session, from_seq) = (self.row.id.clone(), first.seq);
        self.ctx
            .store
            .run(move |c| store_chat::truncate_turns(c, &session, from_seq))
            .await?;
        self.turns.truncate(index);
        self.handles.clear();
        self.client_ids.retain(|_, turn| !removed.contains(turn));
        for turn in &removed {
            self.requested.remove(turn);
            self.round_base.remove(turn);
            self.tool_marks.remove(turn);
            self.snapshotted.remove(turn);
        }
        self.emit(EventKind::TurnsRemoved { from_seq });
        self.touch().await;
        let accepted = match resend {
            Some((text, context, agent)) => Some(
                self.on_send(
                    SendMessage {
                        session_id: self.row.id.clone(),
                        text,
                        agent,
                        client_message_id: None,
                        context,
                    },
                    None,
                )
                .await?,
            ),
            None => None,
        };
        Ok(RewindOutcome {
            result: RewindResult {
                removed_turns: u32::try_from(removed.len()).unwrap_or(u32::MAX),
                user_text: first.user_text,
                context: first.context,
                files,
            },
            accepted,
        })
    }

    /// Keeps the failed turn (and the agent's native session) and asks the agent to carry on.
    async fn continue_turn(
        &mut self,
        failed: harness_core_api::TurnView,
        agent: Option<AgentId>,
    ) -> ApiResult<RewindOutcome> {
        let message = SendMessage {
            session_id: self.row.id.clone(),
            text: CONTINUE_PROMPT.to_owned(),
            agent,
            client_message_id: None,
            context: Vec::new(),
        };
        let accepted = self.on_send(message, None).await?;
        Ok(RewindOutcome {
            result: RewindResult {
                removed_turns: 0,
                user_text: failed.user_text,
                context: failed.context,
                files: None,
            },
            accepted: Some(accepted),
        })
    }

    pub(super) async fn restore_for_rewind(
        &self,
        removed: &[TurnId],
    ) -> ApiResult<FileRestoreReport> {
        let workspace = std::path::PathBuf::from(&self.row.workspace_path);
        match files::rewind_trees(&self.ctx, &workspace, removed).await? {
            Ok(trees) => {
                let plan = files::plan(&workspace, &trees).await?;
                Ok(files::apply(&workspace, &trees, &plan, None, false).await)
            }
            Err(reason) => Ok(FileRestoreReport {
                applied: Vec::new(),
                skipped: vec![(String::new(), reason)],
            }),
        }
    }

    pub(crate) async fn on_apply_files(
        &mut self,
        request: ApplyFileChanges,
    ) -> ApiResult<FileRestoreReport> {
        if self.busy() {
            return Err(self.busy_error());
        }
        if self.turn(&request.turn_id).is_none() {
            return Err(ApiError::NotFound(format!(
                "turn {} in session {}",
                request.turn_id, self.row.id
            )));
        }
        let workspace = std::path::PathBuf::from(&self.row.workspace_path);
        let trees =
            match files::turn_trees(&self.ctx, &workspace, &request.turn_id, request.direction)
                .await?
            {
                Ok(trees) => trees,
                Err(reason) => {
                    return Err(ApiError::InvalidArgument(format!(
                        "file changes are not available: {reason}"
                    )));
                }
            };
        let plan = files::plan(&workspace, &trees).await?;
        let report = files::apply(
            &workspace,
            &trees,
            &plan,
            request.paths.as_deref(),
            request.include_unsafe,
        )
        .await;
        let (turn, undone) = (
            request.turn_id.clone(),
            request.direction == ChangeDirection::Undo,
        );
        self.ctx
            .store
            .run(move |c| store_chat::set_undone(c, &turn, undone))
            .await?;
        self.emit(EventKind::FilesRestored {
            turn_id: request.turn_id,
            direction: request.direction,
        });
        Ok(report)
    }

    /// `available_commands_update` from any native session of this actor (also before Ready).
    pub(crate) async fn on_commands_update(&mut self, native: &str, update: &Value) {
        let agent = self
            .handles
            .iter()
            .find(|(_, h)| h.native == native)
            .map(|(a, _)| a.clone())
            .or_else(|| {
                self.running
                    .as_ref()
                    .filter(|r| r.native.as_deref().map_or(true, |n| n == native))
                    .map(|r| r.agent.clone())
            });
        let Some(agent) = agent else {
            return;
        };
        self.ctx
            .chat
            .record_commands(&self.ctx, &agent, commands_of(update))
            .await;
    }

    /// ACP `plan` and `usage_update` of the running turn.
    pub(crate) async fn on_progress_update(
        &mut self,
        turn_id: &TurnId,
        agent: &AgentId,
        update: &Value,
    ) {
        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("plan") => {
                let entries = plan_entries(update);
                if let Some(turn) = self.turn_mut(turn_id) {
                    turn.plan.clone_from(&entries);
                }
                self.append_event(turn_id, "plan", json!(entries)).await;
                self.emit(EventKind::PlanUpdated {
                    turn_id: turn_id.clone(),
                    agent: agent.clone(),
                    entries,
                });
            }
            Some("usage_update") => {
                let Some(mut usage) = context_usage(update) else {
                    return;
                };
                // 修复：Claude Code 报的是模型原生窗口（1M），实际按 settings.json 里该模型的
                // autoCompactWindow 压缩；上下文面板和 Harness 的自动压缩都按这个窗口算。
                if let Some(model) = super::window::usage_model(update) {
                    let env = self.ctx.specs.get(agent).map(|s| s.env.as_slice());
                    if let Some(dir) = super::window::config_dir(env.unwrap_or_default()) {
                        usage.size = super::window::effective_size(&dir, model, usage.size).await;
                    }
                }
                if let Some(turn) = self.turn_mut(turn_id) {
                    turn.context_usage = Some(usage.clone());
                }
                self.emit(EventKind::UsageUpdated {
                    agent: agent.clone(),
                    usage,
                });
            }
            _ => {}
        }
    }
}
