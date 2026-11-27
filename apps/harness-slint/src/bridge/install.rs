use super::{
    archive_session, board, dock, fleet, history, install_open_folder, mpsc, prd, shown_row,
    thumbs, turns, with_state, work, AgentId, AppWindow, ChatItem, ChatView, Command,
    ComponentHandle, FileReq, FilterModel, HashMap, Model, ModelRc, PanelNav, Path, Rc, Settings,
    SharedString, Theme, UiState, VecModel, CHAT_VIEW, STATE,
};

/// Workspace path, name and initial shown in the side pane, the project chip and the title bar.
pub(super) fn show_workspace(window: &AppWindow, workspace: &Path) {
    window.set_workspace(SharedString::from(workspace.to_string_lossy().as_ref()));
    let name = workspace
        .file_name()
        .map_or_else(|| workspace.to_string_lossy(), |n| n.to_string_lossy());
    window.set_workspace_name(SharedString::from(name.as_ref()));
    window.set_workspace_initial(SharedString::from(
        name.chars()
            .next()
            .map_or_else(|| "H".to_owned(), |c| c.to_uppercase().collect::<String>()),
    ));
}

pub fn install(
    window: &AppWindow,
    cmd: mpsc::UnboundedSender<Command>,
    files_tx: mpsc::UnboundedSender<FileReq>,
    workspace: &Path,
) {
    let sessions = Rc::new(VecModel::default());
    let agents = Rc::new(VecModel::default());
    let fallback = Rc::new(VecModel::default());
    let chat = Rc::new(VecModel::default());
    let files = Rc::new(VecModel::default());
    let queue = Rc::new(VecModel::default());
    window.set_sessions(ModelRc::from(sessions.clone()));
    window.set_agents(ModelRc::from(agents.clone()));
    window.set_fallback(ModelRc::from(fallback.clone()));
    let view: Rc<ChatView> = Rc::new(FilterModel::new(
        chat.clone(),
        shown_row as fn(&ChatItem) -> bool,
    ));
    CHAT_VIEW.with(|v| *v.borrow_mut() = Some(view.clone()));
    window.set_chat(ModelRc::from(view));
    window.set_files(ModelRc::from(files.clone()));
    window.set_queue(ModelRc::from(queue.clone()));
    // 修复：启动时不显示上次的工作区（名称/路径），直到打开会话或文件夹为止。
    show_workspace(window, Path::new(""));
    let mono = if cfg!(target_os = "windows") {
        "Consolas"
    } else if cfg!(target_os = "macos") {
        "Menlo"
    } else {
        "DejaVu Sans Mono"
    };
    window.global::<Theme>().set_mono_font(mono.into());
    // Agent A's panels ask the side pane to switch tabs (a git change row opens the Diff tab).
    let weak = window.as_weak();
    crate::live::install(window);
    crate::wallpaper::install(window);
    let weak_speed = window.as_weak();
    window.global::<crate::ui::ChatUi>().on_speed_tick(move || {
        if let Some(window) = weak_speed.upgrade() {
            crate::speed::tick(&window);
        }
    });
    window
        .global::<crate::ui::ChatUi>()
        .on_turn_secs(crate::turn_clock::shown_secs);
    crate::live::set_workspace(workspace);
    window.global::<PanelNav>().on_open_tab(move |id| {
        if let Some(window) = weak.upgrade() {
            window.set_side_tab(id);
        }
    });
    let state = UiState {
        window: window.clone_strong(),
        cmd,
        files_tx,
        sessions,
        agents,
        fallback,
        chat,
        files,
        queue,
        file_paths: Vec::new(),
        active: None,
        opened_title: String::new(),
        rows: HashMap::new(),
        thumbs: thumbs::Thumbs::default(),
        texts: HashMap::new(),
        blocks: HashMap::new(),
        agent_list: Vec::new(),
        disabled: Vec::new(),
        settings: Settings::default(),
        summaries: Vec::new(),
        sticky: String::new(),
        pending_send: None,
        perm_choices: Vec::new(),
        dock_state: dock::DockState::default(),
        workspace: workspace.to_path_buf(),
        draft: super::draft::Draft::new(workspace),
        turn: turns::TurnState::default(),
        board: board::BoardState::default(),
        history: history::History::default(),
        fleet_view: fleet::FleetView::default(),
        work: work::WorkState::default(),
    };
    STATE.with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            *guard = Some(state);
        }
    });

    wire_callbacks(window);
    install_open_folder(window);
    turns::wire(window);
    history::wire(window);
    dock::wire(window);
    board::wire(window);
    prd::wire(window);
    work::wire(window);
    super::draft::wire(window);
    with_state(|s| s.load_groups());
}

#[allow(clippy::too_many_lines)]
fn wire_callbacks(window: &AppWindow) {
    // Antigravity 式：New session 先进入草稿（选项目/检出），第一条消息才创建会话。
    window.on_new_session(|| {
        with_state(|s| s.start_draft(false, super::draft::DraftTarget::Current));
    });
    window.on_new_worktree_session(|| {
        with_state(|s| s.start_draft(true, super::draft::DraftTarget::Current));
    });
    window.on_archive_session(|id| archive_session(id.to_string()));
    window.on_open_session(|id| {
        with_state(|s| {
            s.send(Command::OpenSession {
                session: id.to_string(),
            });
        });
    });
    window.on_send_draft(|text| {
        with_state(|s| {
            // 长粘贴并入正文；文件、图片、会话 chip 作为 ACP 上下文（src/chat），并记录提示词历史。
            let (text, mut context) = crate::chat::prepare_send(&text);
            if text.trim().is_empty() && context.is_empty() {
                return;
            }
            // 正在编辑队列消息：Send 改为 EditQueued（队列由 hub 持有，结果以下一次 Queue 事件为准）。
            if s.send_edit(text.clone()) {
                return;
            }
            if let Some(session) = s.active.clone() {
                s.add_modes(&session, &mut context);
                s.send_message(session, text, context);
            } else {
                s.take_draft_modes(&mut context);
                s.pending_send = Some((text, context));
                let target = s.draft_target();
                s.new_session(s.draft.worktree, Some(target));
            }
        });
    });
    window.on_stop(|| {
        with_state(|s| {
            if let Some(session) = s.active.clone() {
                s.send(Command::Cancel { session });
            }
        });
    });
    window.on_cancel_queued(|id| {
        with_state(|s| {
            if let Some(session) = s.active.clone() {
                s.send(Command::CancelQueued {
                    session,
                    id: id.to_string(),
                });
            }
        });
    });
    window.on_choose_agent(|agent| {
        with_state(|s| {
            let agent = agent.to_string();
            if let Some(session) = s.active.clone() {
                s.send(Command::SetStickyAgent { session, agent });
            } else {
                s.sticky = agent;
                s.refresh_agents();
            }
        });
    });
    window.on_permission(|request, allow| {
        with_state(|s| {
            if let Ok(request) = u64::try_from(request) {
                s.answer_permission(request, allow);
            }
        });
    });
    window.on_choose_permission(|request, option_id| {
        with_state(|s| {
            if let (Some(session), Ok(request)) = (s.active.clone(), u64::try_from(request)) {
                s.send(Command::ChoosePermission {
                    session,
                    request,
                    option_id: option_id.to_string(),
                });
            }
        });
    });

    window.on_move_fallback(|index, delta| {
        with_state(|s| {
            let visible: Vec<AgentId> = (0..s.fallback.row_count())
                .filter_map(|i| s.fallback.row_data(i))
                .map(|r| r.id.to_string())
                .collect();
            let (Ok(from), Ok(to)) = (usize::try_from(index), usize::try_from(index + delta))
            else {
                return;
            };
            let (Some(a), Some(b)) = (visible.get(from), visible.get(to)) else {
                return;
            };
            let mut order = s.full_order();
            if let (Some(x), Some(y)) = (
                order.iter().position(|id| id == a),
                order.iter().position(|id| id == b),
            ) {
                order.swap(x, y);
                s.send(Command::SetFallbackOrder(order));
            }
        });
    });
    window.on_toggle_file(|index| {
        with_state(|s| {
            if let Ok(index) = usize::try_from(index) {
                s.toggle_file(index);
            }
        });
    });
    window.on_open_link(|url| crate::chat::open_link(&url));
}
