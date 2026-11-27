//! UI-thread side: owns the Slint models, mirrors core events, turns callbacks into core commands.
//!
//! Lives in a `thread_local` on the UI thread; the core thread reaches it only through
//! `slint::invoke_from_event_loop` (spec §3 "Threading").

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use slint::{ComponentHandle, FilterModel, Model, ModelRc, SharedString, VecModel};
use tokio::sync::mpsc;

use crate::core::{
    AgentId, AgentState, AgentStatus, Command, Event, Item, ItemId, Notice, QueuedMessage,
    SessionId, SessionSummary, Settings, SettingsCall, ToolStatus, TurnStatus,
};
use crate::files::{icon_index, Entries, FileReq, ListDir};
use crate::markdown;
use crate::ui::{
    AgentRow, AppWindow, ChatItem, FallbackRow, FileRow, MdBlock, PanelNav, PermOption, QueueRow,
    SessionRow, Theme,
};
use harness_core_api::{ContextItem, PermissionOptionKind};

mod api;
mod board;
mod dock;
mod draft;
mod events;
mod export;
mod fleet;
mod groups;
mod history;
mod install;
mod items;
mod media_refs;
mod prd;
mod pump;
mod session;
mod thumbs;
mod turns;
mod work;

use api::archive_session;
pub use api::{
    agent_names, archive, choose_agent, install_open_folder, open_session, orchestra_agents,
    refresh_usage_parts, request_agent_options, schedule_action, send, session_action,
    show_earlier, teardown, transcript, workspace_path, TranscriptRow,
};
pub use install::install;
use install::show_workspace;
pub use pump::pump;
pub(crate) use thumbs::gallery_of;

struct UiState {
    window: AppWindow,
    cmd: mpsc::UnboundedSender<Command>,
    files_tx: mpsc::UnboundedSender<FileReq>,
    sessions: Rc<VecModel<SessionRow>>,
    agents: Rc<VecModel<AgentRow>>,
    fallback: Rc<VecModel<FallbackRow>>,
    chat: Rc<VecModel<ChatItem>>,
    files: Rc<VecModel<FileRow>>,
    queue: Rc<VecModel<QueueRow>>,
    file_paths: Vec<PathBuf>,
    active: Option<SessionId>,
    /// Title from the open session's own summary: a subagent session (§9.8) is not in `summaries`.
    opened_title: String,
    rows: HashMap<ItemId, usize>,
    texts: HashMap<ItemId, String>,
    blocks: HashMap<ItemId, Rc<VecModel<MdBlock>>>,
    agent_list: Vec<AgentStatus>,
    disabled: Vec<AgentId>,
    settings: Settings,
    summaries: Vec<SessionSummary>,
    sticky: AgentId,
    /// Message typed before any session existed: sent once the created session opens.
    pending_send: Option<(String, Vec<ContextItem>)>,
    workspace: PathBuf,
    /// Project and checkout of the next new session (`bridge/draft.rs`).
    draft: draft::Draft,
    /// Phase 9.2 view state (thinking rows, groups, schedules, change plans): `bridge/turns.rs`.
    turn: turns::TurnState,
    /// Options of the shown permission request (`Event::PermissionChoices`), in the agent's order.
    perm_choices: Vec<(String, PermissionOptionKind)>,
    /// Round 3 dock state (queue edit, effort option id, failure banner): `bridge/dock.rs`.
    dock_state: dock::DockState,
    board: board::BoardState,
    thumbs: thumbs::Thumbs,
    history: history::History,
    fleet_view: fleet::FleetView,
    work: work::WorkState,
}

thread_local! {
    static STATE: RefCell<Option<UiState>> = const { RefCell::new(None) };
    static REVEALED: Cell<i32> = const { Cell::new(-1) };
    static CHAT_VIEW: RefCell<Option<Rc<ChatView>>> = const { RefCell::new(None) };
}

type ChatView = FilterModel<Rc<VecModel<ChatItem>>, fn(&ChatItem) -> bool>;

fn shown_row(row: &ChatItem) -> bool {
    !row.folded || row.id == REVEALED.with(Cell::get)
}

pub fn reveal_row(id: i32) {
    if REVEALED.with(|r| r.replace(id)) != id {
        CHAT_VIEW.with(|v| {
            if let Some(view) = v.borrow().as_ref() {
                view.reset();
            }
        });
    }
}

pub fn shown_index(id: i32) -> i32 {
    CHAT_VIEW.with(|v| {
        v.borrow()
            .as_ref()
            .and_then(|view| view.iter().position(|row| row.id == id))
            .map_or(-1, |at| i32::try_from(at).unwrap_or(-1))
    })
}

fn with_state(f: impl FnOnce(&mut UiState)) {
    STATE.with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            if let Some(state) = guard.as_mut() {
                f(state);
            }
        }
    });
}

pub fn set_disabled_agents(ids: Vec<AgentId>) {
    with_state(|s| {
        if s.disabled != ids {
            s.disabled = ids;
            s.refresh_agents();
        }
    });
}

fn turn_status(status: TurnStatus) -> i32 {
    match status {
        TurnStatus::Streaming => 0,
        TurnStatus::Done => 1,
        TurnStatus::Cancelled => 2,
        TurnStatus::Interrupted => 3,
        TurnStatus::Failed => 4,
    }
}

fn tool_status(status: ToolStatus) -> i32 {
    match status {
        ToolStatus::Running => 0,
        ToolStatus::AwaitingPermission => 1,
        ToolStatus::Done => 2,
        ToolStatus::Denied => 3,
        ToolStatus::Failed => 4,
    }
}

fn item_id(id: ItemId) -> i32 {
    i32::try_from(id).unwrap_or(i32::MAX)
}

impl UiState {
    fn send(&self, command: Command) {
        // core 线程已退出只会发生在关停阶段，此时丢弃命令是正确行为。
        let _ = self.cmd.send(command);
    }

    fn agent_name(&self, id: &str) -> String {
        self.agent_list
            .iter()
            .find(|a| a.id == id)
            .map_or_else(|| id.to_string(), |a| a.name.clone())
    }

    fn agent_row(agent: &AgentStatus) -> AgentRow {
        let (state, reset) = match &agent.state {
            AgentState::Available => (0, String::new()),
            AgentState::Unavailable => (1, String::new()),
            AgentState::AdapterMissing => (3, String::new()),
            AgentState::Exhausted { reset } => (2, reset.clone().unwrap_or_default()),
        };
        AgentRow {
            id: agent.id.as_str().into(),
            name: agent.name.as_str().into(),
            state,
            reset: reset.into(),
            claude: agent.id.as_str() == "claude-code"
                || agent.id.as_str().starts_with("claude-code-"),
        }
    }

    fn full_order(&self) -> Vec<AgentId> {
        let mut order: Vec<AgentId> = self
            .settings
            .fallback_order
            .iter()
            .filter(|id| self.agent_list.iter().any(|a| &a.id == *id))
            .cloned()
            .collect();
        for agent in &self.agent_list {
            if !order.contains(&agent.id) {
                order.push(agent.id.clone());
            }
        }
        order
    }

    fn refresh_agents(&mut self) {
        // 选择器与设置页都按 fallback 顺序展示，未出现在顺序里的 agent 追加在末尾。
        let mut ordered: Vec<&AgentStatus> = self
            .settings
            .fallback_order
            .iter()
            .filter_map(|id| self.agent_list.iter().find(|a| &a.id == id))
            .collect();
        for agent in &self.agent_list {
            if !ordered.iter().any(|a| a.id == agent.id) {
                ordered.push(agent);
            }
        }
        let rows: Vec<AgentRow> = ordered
            .into_iter()
            .filter(|a| !self.disabled.contains(&a.id))
            .map(Self::agent_row)
            .collect();
        if self.active.is_none() && self.disabled.contains(&self.sticky) {
            if let Some(first) = rows.iter().find(|r| r.state == 0) {
                self.sticky = first.id.to_string();
            }
        }
        self.fallback.set_vec(
            rows.iter()
                .map(|r| FallbackRow {
                    id: r.id.clone(),
                    name: r.name.clone(),
                    state: r.state,
                })
                .collect::<Vec<_>>(),
        );
        self.agents.set_vec(
            rows.into_iter()
                .filter(|r| r.state == 0 || r.state == 2)
                .collect::<Vec<_>>(),
        );
        self.window.set_sticky_agent(self.sticky.as_str().into());
        self.window
            .set_sticky_name(self.agent_name(&self.sticky).into());
        self.show_modes();
    }

    fn refresh_sessions(&mut self) {
        // 侧栏分区（置顶、分组、时间线）由 turns::session_rows 生成；上一个/下一个会话按显示顺序。
        let (rows, order): (Vec<SessionRow>, _) = self.session_rows();
        self.sessions.set_vec(rows);
        crate::chat::sessions_changed(order, self.active.as_deref());
        let title = self
            .active
            .as_ref()
            .and_then(|id| self.summaries.iter().find(|s| &s.id == id))
            .map(|s| s.title.clone())
            .or_else(|| self.active.as_ref().map(|_| self.opened_title.clone()))
            .unwrap_or_default();
        self.window.set_active_title(title.into());
    }
}

/// File listing reply (UI thread).
pub fn files_listed(dir: &Path, depth: i32, entries: Entries) {
    with_state(|state| {
        let (insert_at, parent) = if depth == 0 {
            state.files.set_vec(Vec::new());
            state.file_paths.clear();
            (0, None)
        } else {
            // 父目录可能在请求期间被折叠或移动，按路径重新定位，找不到就丢弃结果。
            let Some(parent) = state.file_paths.iter().position(|p| p == dir) else {
                return;
            };
            if state.files.row_data(parent).is_some_and(|r| r.expanded) {
                return;
            }
            (parent + 1, Some(parent))
        };
        for (offset, (name, is_dir)) in entries.into_iter().enumerate() {
            state.files.insert(
                insert_at + offset,
                FileRow {
                    name: name.as_str().into(),
                    depth,
                    is_dir,
                    expanded: false,
                    icon: icon_index(&name, is_dir),
                },
            );
            state.file_paths.insert(insert_at + offset, dir.join(&name));
        }
        if let Some(parent) = parent {
            if let Some(row) = state.files.row_data(parent) {
                state.files.set_row_data(
                    parent,
                    FileRow {
                        expanded: true,
                        ..row
                    },
                );
            }
        }
    });
}

/// Installs the UI state and wires every window callback to a core command.
/// System UI font with Simplified Chinese glyphs. Windows' default fallback picks a Japanese font for Han
/// characters, which lacks simplified-only forms; an unknown family falls back to Slint's default.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn ui_font_family(zh: bool, choice: &str) -> &'static str {
    if !zh && choice == "inter" {
        "Inter"
    } else if !zh && choice != "system" {
        "Plus Jakarta Sans"
    } else if cfg!(windows) {
        if zh {
            "Microsoft YaHei UI"
        } else {
            "Segoe UI"
        }
    } else if cfg!(target_os = "macos") {
        "PingFang SC"
    } else {
        "Noto Sans CJK SC"
    }
}
