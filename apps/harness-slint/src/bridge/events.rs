use super::{
    dock, history, one_line, thumbs, tool_status, turn_status, ComponentHandle, Event, Item, Model,
    UiState,
};

impl UiState {
    // One flat match over the event kinds; each arm is short.
    #[allow(clippy::too_many_lines)]
    pub(super) fn apply(&mut self, event: Event) {
        match event {
            Event::Settings(settings) => self.apply_settings(settings),
            Event::Agents(list) => self.apply_agents(list),
            Event::AgentOptions {
                agent,
                modes,
                models,
                efforts,
                known,
            } => {
                // 未知（agent 尚未启动过）时列表为空，不覆盖已有的选项。
                if known {
                    crate::settings::agent_options(&agent, modes, models, efforts);
                }
            }
            Event::AgentsOverview(rows) => crate::settings::agents_overview(rows),
            Event::AgentTested(result) => crate::settings::agent_tested(result),
            Event::Accounts(result) => {
                crate::plan_usage::accounts(&result);
                crate::settings::accounts(result);
            }
            Event::Providers(result) => crate::settings::providers(result),
            Event::AgyAccounts { sign_in, result } => {
                crate::plan_usage::agy_accounts(&result);
                crate::settings::agy::accounts(sign_in.as_deref(), result);
            }
            Event::SetupStatus(result) => crate::setup::status(result),
            Event::AdapterInstall {
                agent,
                status,
                line,
                message,
            } => {
                if agent == "gh" {
                    crate::inbox::gh_install_event(status, message.clone());
                }
                crate::setup::install_event(&agent, status, line, message);
            }
            Event::Sessions(list) => {
                crate::settings::sessions_changed();
                // 当前会话被归档或删除后不再出现在列表里；hub 不会替界面关闭它，这里清空视图。
                // 子代理会话本来就不在列表中（§9.8，list_sessions 隐藏子会话）：打开它时不能当作已删除而清空。
                let subagent_open = !self.ui().get_parent_id().is_empty();
                if !subagent_open
                    && self.active.as_ref().is_some_and(|id| {
                        self.summaries.iter().any(|s| &s.id == id)
                            && !list.iter().any(|s| &s.id == id)
                    })
                {
                    self.clear_session();
                }
                self.summaries = list;
                self.refresh_sessions();
                // 打开着的会话不应显示未读：hub 在回合结束时标记未读，这里立即清除。
                // 修复：用户在打开的会话上“标为未读”后，下一次 Sessions 事件会立刻把它清掉；
                // 用户主动标记的会话在切换到别的会话之前保持未读。
                if let Some(active) = self.active.clone() {
                    let kept = self.turn.keep_unread.as_deref() == Some(active.as_str());
                    if !kept && self.summaries.iter().any(|s| s.id == active && s.unread) {
                        self.set_unread(&active, false);
                    }
                }
            }
            Event::SessionOpened {
                session,
                items,
                sticky,
                queued,
                paused,
            } => self.open_session(&session, &items, sticky, &queued, paused),
            Event::ItemAdded { session, id, item } => {
                if self.is_active(&session) {
                    if let Item::Tool { status, .. } = &item {
                        crate::speed::tool(id, *status);
                    }
                    self.push_item(id, &item);
                }
            }
            Event::TextDelta { session, id, text } => {
                if self.is_active(&session) {
                    crate::speed::text(&self.window, Some(id), text.chars().count());
                    self.append_text(id, &text);
                }
            }
            Event::TurnEnded {
                session,
                id,
                status,
            } => {
                if self.is_active(&session) {
                    self.update_row(id, |row| row.status = turn_status(status));
                    self.answer_media(id);
                    self.turn_ended(id);
                    // Generation speed of the turn, estimated from its text; an exact count may follow
                    // with the session meta.
                    if let Some((row, label)) = crate::speed::finish(&self.window) {
                        self.update_row(row, |r| r.speed = label.as_str().into());
                    }
                    self.work_turn_finished();
                }
            }
            Event::ToolUpdated {
                session,
                id,
                status,
            } => {
                if self.is_active(&session) {
                    crate::speed::tool(id, status);
                    self.update_row(id, |row| row.status = tool_status(status));
                }
            }
            Event::ToolTitle { session, id, title } => {
                let changed = self.is_active(&session)
                    && self
                        .rows
                        .get(&id)
                        .and_then(|&index| self.chat.row_data(index))
                        .is_some_and(|row| row.title.as_str() != one_line(&title));
                if changed {
                    let image: Vec<String> = thumbs::tool_image(&title, &self.workspace)
                        .into_iter()
                        .collect();
                    let (images, more) = self.row_images(id, &image);
                    let work = self.work_detect_tool(&title);
                    self.update_row(id, |row| {
                        row.title = one_line(&title).into();
                        row.images = images;
                        row.images_more = more;
                        if let Some((slug, wtitle, kind, details)) = work {
                            row.work_slug = slug.into();
                            row.work_title = wtitle.into();
                            row.work_kind = kind.into();
                            row.work_details = details.into();
                        }
                    });
                }
            }
            Event::PermissionRequested {
                session,
                request,
                title,
                detail,
            } => {
                if self.is_active(&session) {
                    self.window
                        .set_perm_request(i32::try_from(request).unwrap_or(i32::MAX));
                    self.window.set_perm_title(title.into());
                    self.window.set_perm_detail(detail.into());
                    // 选项随后由 PermissionChoices 送达；先清掉上一个请求的选项，避免把旧 id 回答给新请求。
                    self.set_perm_choices(Vec::new());
                    self.window.set_perm_visible(true);
                }
            }
            Event::PermissionChoices {
                session,
                request,
                options,
            } => {
                if self.is_active(&session)
                    && i64::from(self.window.get_perm_request())
                        == i64::try_from(request).unwrap_or(-1)
                {
                    self.set_perm_choices(
                        options
                            .into_iter()
                            .map(|o| (o.id, o.name, o.kind))
                            .collect(),
                    );
                }
            }
            Event::PermissionCleared { session, request } => {
                if self.is_active(&session)
                    && i64::from(self.window.get_perm_request())
                        == i64::try_from(request).unwrap_or(-1)
                {
                    self.window.set_perm_visible(false);
                }
            }
            Event::Sticky { session, agent } => {
                if self.is_active(&session) {
                    self.sticky = agent;
                    self.refresh_agents();
                    self.load_commands();
                    self.load_skills();
                }
            }
            Event::Running { session, running } => {
                if self.is_active(&session) {
                    // A turn starts: its generation speed is counted from zero. 修复：切回正在运行的会话时
                    // 不再归零——计时和 tok/s 都按回合开始时间识别同一回合，继续原来的计数。
                    if running {
                        crate::turn_clock::ensure(&session);
                        crate::speed::start(&self.window, crate::turn_clock::start_of(&session));
                    } else {
                        crate::turn_clock::ended(&session);
                    }
                    self.window.set_running(running);
                    // 新回合开始：上一次失败的横幅过时了。
                    if running {
                        self.dismiss_failure();
                    }
                }
            }
            Event::Queue { session, items } => {
                if self.is_active(&session) {
                    self.set_queue(&items);
                    self.queue_changed(&items);
                }
            }
            Event::Paused { session, paused } => {
                if self.is_active(&session) {
                    self.window.set_paused(paused);
                }
            }
            Event::Error(message) => {
                if !self.failure_repeats(&message) {
                    self.window.set_error_text(message.into());
                }
            }
            Event::ToolDetail {
                session,
                id,
                kind,
                content,
                locations,
                raw_input,
                raw_output,
            } => {
                if self.is_active(&session) && self.history.hides(id) {
                    self.history.keep_detail(
                        id,
                        history::Detail {
                            kind,
                            content,
                            locations,
                            raw_input,
                            raw_output,
                        },
                    );
                } else if self.is_active(&session) {
                    self.tool_detail(
                        id,
                        kind.as_deref(),
                        &content,
                        &locations,
                        raw_input.as_deref(),
                        raw_output.as_deref(),
                    );
                    // Only a running turn moves the Live view (opening a session replays every tool call).
                    if self.window.get_running() {
                        crate::live::touch(&self.window, kind.as_deref(), &content, &locations);
                    }
                }
            }
            Event::SubagentTool {
                root,
                title,
                kind,
                content,
                locations,
            } => {
                if self.is_active(&root) {
                    crate::live::touch_subagent(
                        &self.window,
                        &title,
                        kind.as_deref(),
                        &content,
                        &locations,
                    );
                }
            }
            Event::Thought { session, text } => {
                if self.is_active(&session) {
                    crate::speed::text(&self.window, None, text.chars().count());
                    self.thought(&text);
                }
            }
            Event::Plan { session, entries } => {
                if self.is_active(&session) {
                    self.plan(&entries);
                }
            }
            Event::Subagents { session, items } => {
                if self.is_active(&session) {
                    self.subagents(&items);
                }
            }
            Event::Fleet(list) => self.fleet(&list),
            Event::BackgroundTasks { session, tasks } => {
                if self.is_active(&session) {
                    self.background_tasks(&tasks);
                }
            }
            Event::ParentLink { session, parent } => {
                if self.is_active(&session) {
                    self.parent_link(parent);
                }
            }
            Event::SessionMeta {
                session,
                worktree,
                cost,
                tokens,
            } => {
                if self.is_active(&session) {
                    self.session_meta(worktree.as_ref(), cost.as_ref());
                    // The agent reported the output tokens of the turn that just ended: the exact speed
                    // replaces the estimate.
                    if let Some((row, label)) = tokens.and_then(crate::speed::reported) {
                        self.update_row(row, |r| r.speed = label.as_str().into());
                    }
                }
            }
            Event::Usage { session, usage } => {
                if self.is_active(&session) {
                    self.usage(&usage);
                }
            }
            Event::Commands { agent } => {
                if agent == self.sticky {
                    self.load_commands();
                }
            }
            Event::Groups => self.load_groups(),
            Event::Board { workspace_key } => self.board_event(&workspace_key),
            Event::Inbox => crate::inbox::changed(),
            Event::Schedules => {
                if self
                    .window
                    .global::<crate::ui::ChatUi>()
                    .get_schedules_open()
                {
                    self.load_schedules();
                }
            }
            Event::FilesRestored { session, item, .. } => self.files_restored(&session, item),
            Event::SessionOptions {
                session,
                mode,
                model,
                effort,
                effort_id,
                model_id,
                modes,
                models,
                efforts,
                pending,
                locked,
                machine,
                ..
            } => {
                if self.is_active(&session) {
                    self.apply_options(Some(dock::Options {
                        mode,
                        model,
                        effort,
                        effort_id,
                        model_id,
                        modes,
                        models,
                        efforts,
                        pending,
                        locked,
                        machine,
                    }));
                }
            }
            Event::Failure {
                session,
                kind,
                message,
                details,
                trace,
                retry,
                ..
            } => self.failure(dock::FailureEvent {
                session,
                kind,
                message,
                details,
                trace,
                retry,
            }),
        }
    }
}
