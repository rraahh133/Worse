use super::{
    api, emit, permission_detail, snapshot_turn, summary, tail_start, tool_detail, tool_title,
    with_content_warnings, Adapter, Event, Item, Notice, PendingPermission, PermissionChoice,
    PermissionRequestView, ToolStatus, TurnMap, View,
};

impl Adapter {
    #[allow(clippy::too_many_lines)]
    pub(super) async fn open(&mut self, session: &str, all: bool) {
        let detail = match self.core.open_session(api::SessionId::new(session)).await {
            Ok(detail) => detail,
            Err(error) => {
                self.report("open session", &error);
                return;
            }
        };
        self.permissions.clear();
        // The chat now shows this session: the Diff panel's "Turn changes" follows it.
        crate::panels::session_opened(detail.summary.id.clone());
        let mut view = View::new(detail.summary.id.clone());
        view.since = detail.event_seq;
        view.last_failed = detail
            .turns
            .last()
            .filter(|t| t.status == api::TurnStatus::Failed)
            .map(|t| t.id.clone());
        let mut items = Vec::new();
        let mut running = false;
        let mut paused = false;
        let start = if all { 0 } else { tail_start(&detail.turns) };
        if let Some(first) = detail.turns.first().filter(|_| start > 0) {
            let id = view.alloc_in(&first.id);
            items.push((
                id,
                Item::Notice {
                    notice: Notice::Earlier { count: start },
                },
            ));
        }
        for (index, turn) in detail.turns.iter().enumerate() {
            running |= turn.status == api::TurnStatus::Running;
            paused |= turn.status == api::TurnStatus::Paused;
            if index < start {
                view.turns.insert(turn.id.clone(), TurnMap::default());
                continue;
            }
            if detail.queue.contains(&turn.id) {
                // 排队中的消息只显示在输入框上方的队列里（可取消），开始运行时才进入对话。
                view.turns.insert(turn.id.clone(), TurnMap::default());
                continue;
            }
            if turn.stop_reason.as_deref() == Some("steered") {
                view.turns.insert(turn.id.clone(), TurnMap::default());
                continue;
            }
            snapshot_turn(&mut view, turn, &mut items);
        }
        view.queued = detail
            .queue
            .iter()
            .map(|id| {
                let turn = detail.turns.iter().find(|t| &t.id == id);
                let images =
                    turn.map_or_else(Vec::new, |t| api::ContextItem::image_paths(&t.context));
                if !images.is_empty() {
                    view.queued_images.insert(id.clone(), images);
                }
                (
                    id.clone(),
                    turn.map(|t| t.user_text.clone()).unwrap_or_default(),
                )
            })
            .collect();
        let id = detail.summary.id.0.clone();
        let sticky = detail.summary.active_agent.0.clone();
        self.sticky
            .insert(detail.summary.id.clone(), sticky.clone());
        self.emit(Event::SessionOpened {
            session: summary(&detail.summary),
            items,
            sticky,
            queued: view.queue_items(),
            paused,
        });
        view.running = running;
        // 修复：回合在别的会话显示时开始（或应用刚打开），计时从快照里该回合的创建时间算起，而不是从 0。
        match detail
            .turns
            .iter()
            .find(|t| t.status == api::TurnStatus::Running)
        {
            Some(turn) => {
                crate::turn_clock::started(&id, turn.id.as_str(), Some(turn.created_at));
            }
            None => crate::turn_clock::ended(&id),
        }
        self.emit(Event::Running {
            session: id.clone(),
            running,
        });
        for turn in &detail.turns {
            for tool in &turn.tool_calls {
                if let Some(&item) = view.tools.get(&tool.tool_call_id) {
                    tool_detail(&self.ev, &id, item, tool);
                }
            }
        }
        self.emit(Event::Plan {
            session: id.clone(),
            entries: detail.plan,
        });
        view.subagents.clone_from(&detail.subagents);
        let prompt = detail
            .turns
            .iter()
            .rev()
            .find(|t| !detail.queue.contains(&t.id) && !t.user_text.is_empty())
            .map_or("", |t| t.user_text.as_str());
        self.fleet.set_open(&id, &detail.subagents, prompt);
        self.fleet.set_running(&id, running);
        if let Some(fleet) = self.fleet.changed() {
            self.emit(Event::Fleet(fleet));
        }
        self.emit(Event::Subagents {
            session: id.clone(),
            items: detail.subagents,
        });
        self.emit(Event::BackgroundTasks {
            session: id.clone(),
            tasks: detail.background_tasks,
        });
        let parent = match &detail.summary.parent_id {
            Some(parent) => Some((parent.0.clone(), self.session_title(parent).await)),
            None => None,
        };
        self.emit(Event::ParentLink {
            session: id.clone(),
            parent,
        });
        self.emit(Event::SessionMeta {
            session: id.clone(),
            worktree: detail.summary.worktree.clone(),
            cost: detail.cost.clone(),
            // Opening a session: no turn has just ended.
            tokens: None,
        });
        if let Some(usage) = detail.context_usage {
            self.emit(Event::Usage { session: id, usage });
        }
        self.view = Some(view);
        for request in detail.pending_permissions {
            self.permission_requested(request);
        }
        self.session_options(detail.summary.id.as_str(), false)
            .await;
    }

    pub(super) fn permission_requested(&mut self, request: PermissionRequestView) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let session = view.id.0.clone();
        let tool_id = request.tool.tool_call_id.clone();
        let tool = view.merge_tool(&request.tool);
        let item = if let Some(&item) = view.tools.get(&tool_id) {
            item
        } else {
            let item = view.alloc_in(&request.turn_id);
            view.tools.insert(tool_id.clone(), item);
            emit(
                &self.ev,
                Event::ItemAdded {
                    session: session.clone(),
                    id: item,
                    item: Item::Tool {
                        agent: request.agent.0.clone(),
                        title: tool.title.clone(),
                        status: ToolStatus::AwaitingPermission,
                    },
                },
            );
            item
        };
        let number = self.next_request;
        self.next_request += 1;
        let detail = with_content_warnings(permission_detail(&tool), &tool);
        self.emit(Event::ToolUpdated {
            session: session.clone(),
            id: item,
            status: ToolStatus::AwaitingPermission,
        });
        tool_title(&self.ev, &session, item, &tool.title);
        self.emit(Event::PermissionRequested {
            session: session.clone(),
            request: number,
            title: tool.title.clone(),
            detail,
        });
        self.emit(Event::PermissionChoices {
            session: session.clone(),
            request: number,
            options: request
                .options
                .iter()
                .map(|o| PermissionChoice {
                    id: o.option_id.clone(),
                    name: o.name.clone(),
                    kind: o.kind,
                })
                .collect(),
        });
        tool_detail(&self.ev, &session, item, &tool);
        self.permissions.insert(
            number,
            PendingPermission {
                request_id: request.request_id,
                options: request.options,
                tool_call_id: tool_id,
            },
        );
    }
}
