use super::{
    api, emit_overview, emit_setup_status, prefs, Adapter, Event, EventKind, HubEvent, Pref,
};

impl Adapter {
    pub(super) async fn on_event(&mut self, event: HubEvent) {
        if self.fleet.on_event(&event) {
            if let Some(fleet) = self.fleet.changed() {
                self.emit(Event::Fleet(fleet));
            }
        }
        match &event.kind {
            EventKind::SessionCreated { .. }
            | EventKind::SessionUpdated { .. }
            | EventKind::SessionsChanged => {
                self.refresh_sessions().await;
            }
            EventKind::AgentQuota { .. } => self.refresh_agents().await,
            EventKind::CommandsChanged { agent } => self.emit(Event::Commands {
                agent: agent.0.clone(),
            }),
            EventKind::GroupsChanged => self.emit(Event::Groups),
            EventKind::SchedulesChanged => self.emit(Event::Schedules),
            EventKind::BoardChanged { workspace_key } => self.emit(Event::Board {
                workspace_key: workspace_key.clone(),
            }),
            EventKind::InboxChanged => self.emit(Event::Inbox),
            EventKind::AdapterInstall {
                agent,
                status,
                line,
                message,
                ..
            } => {
                self.emit(Event::AdapterInstall {
                    agent: agent.0.clone(),
                    status: *status,
                    line: line.clone(),
                    message: message.clone(),
                });
                // §9.10：安装结束后重新探测，agent 无需重启即可用。
                if *status != api::AdapterInstallStatus::Running {
                    self.refresh_agents().await;
                    emit_overview(&*self.core, &self.ev).await;
                    emit_setup_status(&*self.core, &self.ev).await;
                }
            }
            _ => {}
        }
        // 修复：工作行的计时只记当前打开会话，切走再回来就从 0 开始；这里为所有会话记下回合开始时间。
        if let Some(session) = &event.session_id {
            match &event.kind {
                EventKind::TurnStarted { turn_id, .. } => {
                    crate::turn_clock::started(session.as_str(), turn_id.as_str(), None);
                }
                EventKind::TurnFinished { .. } | EventKind::TurnPaused { .. } => {
                    crate::turn_clock::ended(session.as_str());
                }
                _ => {}
            }
        }
        // 修复说明：重试/回退/编辑重发后端口会重新打开会话；hub 在快照之前已发布的本会话事件（TurnQueued、
        // TurnStarted…）仍在订阅缓冲里。若照常应用，TurnQueued 会以空映射覆盖快照建立的条目映射并重复添加用户
        // 气泡，之后的 MessageDelta 另起新助手行，快照里的助手行永远停在 Streaming。快照之前的事件已包含在快照中，丢弃。
        let is_open = match (&self.view, &event.session_id) {
            (Some(view), Some(id)) => &view.id == id && event.seq > view.since,
            _ => false,
        };
        if !is_open {
            let open = self.view.as_ref().map(|v| v.id.0.clone());
            if let Some(tool) = open.and_then(|open| self.fleet.subagent_tool(&open, &event)) {
                self.emit(tool);
            }
        }
        if is_open && matches!(event.kind, EventKind::TurnsRemoved { .. }) {
            // 回退/编辑重发/重试删除了回合：已显示的条目和 item→turn 映射都已失效，按快照重建视图。
            if let Some(id) = self.active_id() {
                self.open(&id, false).await;
            }
            return;
        }
        if is_open {
            let options = matches!(
                event.kind,
                EventKind::TurnStarted { .. }
                    | EventKind::TurnFinished { .. }
                    | EventKind::TurnPaused { .. }
                    | EventKind::SessionOptionsChanged { .. }
                    | EventKind::AgentSwitched { .. }
            );
            let meta = match &event.kind {
                EventKind::TurnFinished { .. } => true,
                EventKind::SubagentUpdated { subagent } => {
                    subagent.finished_at.is_some() && !subagent.status.is_active()
                }
                _ => false,
            };
            self.on_session_event(event.kind);
            if options {
                if let Some(id) = self.active_id() {
                    self.session_options(&id, false).await;
                }
            }
            if meta {
                if let Some(id) = self.active_id() {
                    self.session_meta(&id).await;
                }
            }
        }
    }

    /// Emits `Event::SessionOptions` of `session` (R13). Reads the hub's session lists (cached, no agent start).
    pub(super) async fn session_meta(&self, session: &str) {
        if let Ok(detail) = self
            .core
            .open_session(api::SessionId::new(session.to_owned()))
            .await
        {
            let tokens = detail
                .turns
                .last()
                .filter(|turn| turn.finished_at.is_some())
                .and_then(|turn| turn.usage.as_ref())
                .map(|usage| usage.output)
                .filter(|output| *output > 0);
            self.emit(Event::SessionMeta {
                session: session.to_owned(),
                worktree: detail.summary.worktree,
                cost: detail.cost,
                tokens,
            });
        }
    }

    pub(super) async fn session_options(&self, session: &str, pending: bool) {
        let id = api::SessionId::new(session.to_owned());
        let (mode_list, catalog, config, machine) = tokio::join!(
            self.core.list_modes(id.clone()),
            self.core.list_models(id.clone()),
            self.core.list_config_options(id.clone()),
            self.core.machine(id),
        );
        let (mode_list, catalog, config) = match (mode_list, catalog, config) {
            (Ok(m), Ok(n), Ok(c)) => (m, n, c),
            (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
                return self.report("session options", &error);
            }
        };
        let effort = prefs::effort_option(&config.options);
        // No ACP model list: fall back to the agent's `model` config option (newer Claude Code).
        let model_option = if catalog.available.is_empty() {
            prefs::model_option(&config.options)
        } else {
            None
        };
        let locked = self
            .view
            .as_ref()
            .is_some_and(|v| v.id.0 == session && v.running);
        self.emit(Event::SessionOptions {
            session: session.to_owned(),
            agent: mode_list.agent.0.clone(),
            mode: mode_list.current.clone(),
            model: match model_option {
                Some(o) => o.current.clone(),
                None => catalog.current.clone(),
            },
            effort: effort.and_then(|o| o.current.clone()),
            effort_id: effort.map(|o| o.id.clone()),
            model_id: model_option.map(|o| o.id.clone()),
            modes: mode_list
                .available
                .iter()
                .map(|m| (m.id.clone(), m.name.clone()))
                .collect(),
            models: match model_option {
                Some(o) => o
                    .values
                    .iter()
                    .map(|v| (v.value.clone(), v.name.clone()))
                    .collect(),
                None => catalog
                    .available
                    .iter()
                    .map(|m| (m.id.clone(), m.name.clone()))
                    .collect(),
            },
            efforts: effort
                .map(|o| {
                    o.values
                        .iter()
                        .map(|v| (v.value.clone(), v.name.clone()))
                        .collect()
                })
                .unwrap_or_default(),
            known: mode_list.known || catalog.known || config.known,
            pending,
            locked,
            machine: machine.unwrap_or(false),
        });
    }

    /// Mode / model / config option of the session's next prompts: `pending` options, the hub call, then the
    /// result (the hub also emits `SessionOptionsChanged`, which re-sends them).
    pub(super) async fn set_pref(&mut self, session: String, pref: Pref) {
        self.session_options(&session, true).await;
        let id = api::SessionId::new(session.clone());
        let result = match pref {
            Pref::Mode(mode) => self.core.set_mode(id, mode).await,
            Pref::Model(model) => self.core.set_model(id, model).await,
            Pref::Config(option, value) => self.core.set_config_option(id, option, value).await,
            Pref::Machine(on) => self.core.set_machine(id, on).await,
        };
        if let Err(error) = result {
            self.report("session option", &error);
        }
        self.session_options(&session, false).await;
    }

    /// `RetryInit { Some }` re-runs the session's last failed turn (same agent choice as a plain retry).
    pub(super) async fn retry_failed(&mut self, session: Option<String>) {
        let Some(session) = session else {
            // hub 已在运行：重启请求只在启动失败的等待循环里处理（见 `run`）。
            return;
        };
        let turn = self
            .view
            .as_ref()
            .filter(|v| v.id.0 == session)
            .and_then(|v| v.last_failed.clone());
        let Some(turn_id) = turn else {
            return self.emit(Event::Error(format!(
                "session {session} has no failed turn to retry"
            )));
        };
        let request = api::Retry {
            session_id: api::SessionId::new(session),
            turn_id,
            agent: None,
            restore_files: false,
        };
        match self.core.retry_turn(request).await {
            Ok(_) => {
                if let Some(view) = self.view.as_mut() {
                    view.last_failed = None;
                }
            }
            Err(error) => self.report("retry", &error),
        }
    }
}
