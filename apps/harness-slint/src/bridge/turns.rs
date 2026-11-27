//! Phase 9.2 wiring of the agent A events and chat commands (spec `docs/specs/harness/phase-9-parity.md` §7):
//! tool detail and thinking rows (R8), deny with feedback (R9), session actions (R10), groups (R11), the
//! changed-files summary (R14), plan / context meter, agent slash commands (R13) and scheduled prompts (R18).
//!
//! Every reply of a `ChatCall` arrives on a tokio oneshot and is applied in a later `with_state` turn
//! (`slint::spawn_local`), never inside the borrow that sent it.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use harness_core_api::{
    ApiError, BackgroundTask, ChangeDirection, ContextUsage, FileAction, FileChangePlan,
    GroupColor, NewSchedule, PlanEntry, ScheduleKind, ScheduleTarget, ScheduledPrompt,
    SessionGroup, SessionSelection, SubagentStatus, SubagentView, ToolContent, ToolLocation,
    UpdateGroup, UpdateSchedule,
};

use super::{item_id, with_state, UiState};
use crate::core::{ChatCall, Command, Item, ItemId, Reply, SessionId, SettingsCall};
use crate::ui::{
    BackgroundTaskRow, ChangeRow, ChatItem, ChatUi, GroupRow, OutputRow, PlanRow, ScheduleRow,
    SessionRow, SubagentRow, ToolPart, UsagePart,
};

mod changes;
mod dialogs;
mod parts;
mod rows;
mod schedules;
mod sessions;
#[cfg(test)]
mod tests;
mod view;

use parts::{
    diff_counts, peek_parts, plan_rows, schedule_row, thought_preview, tokens, tool_parts,
    THOUGHT_PREVIEW_CHARS,
};
pub(super) use rows::subagent_row;
use rows::{background_row, trails};

/// Longest diff body shown in a tool card (lines); the rest is summarised in one line.
const MAX_DIFF_LINES: usize = 400;

/// What the shared text dialog (`ChatUi.dialog-*`) submits to.
#[derive(Clone, Default)]
pub(super) enum DialogTarget {
    #[default]
    None,
    Rename(SessionId),
    Edit {
        session: SessionId,
        item: ItemId,
    },
    Rewind {
        session: SessionId,
        item: ItemId,
    },
    Deny {
        session: SessionId,
        request: u64,
    },
    Delete(SessionId),
    NewGroup,
    RenameGroup(String),
    DeleteGroup(String),
    /// Remove a workspace (sidebar project, by its key) from the sidebar: archives all its sessions.
    RemoveProject(String),
    NewSchedule,
}

/// Phase 9.2 view state kept next to `UiState` (thinking rows, groups, schedules, change plans).
#[derive(Default)]
pub(super) struct TurnState {
    /// Live thinking row: (synthetic id, row index). Synthetic ids are negative (-2, -3, …).
    thought: Option<(i32, usize)>,
    thought_rows: std::collections::HashMap<i32, usize>,
    next_thought: i32,
    pub(super) groups: Vec<SessionGroup>,
    collapsed: HashSet<String>,
    schedules: Vec<ScheduledPrompt>,
    /// Turn id → assistant item that shows its change summary (the last item of the turn).
    change_turns: std::collections::HashMap<String, ItemId>,
    dialog: DialogTarget,
    /// Session the user marked unread while it was open: not auto-cleared until another session is shown.
    pub(super) keep_unread: Option<String>,
    compact_due: bool,
    compacted: bool,
    outputs: Vec<String>,
    memory_tokens: Option<u64>,
    memory_asked: bool,
    last_usage: Option<(u64, u64)>,
}

impl TurnState {
    pub(super) fn shift_rows(&mut self, by: usize) {
        if let Some((_, index)) = self.thought.as_mut() {
            *index += by;
        }
        for index in self.thought_rows.values_mut() {
            *index += by;
        }
    }
}

const AUTO_COMPACT_AT: f32 = 0.85;

fn measure_memory(workspace: std::path::PathBuf) {
    std::thread::spawn(move || {
        let mut files = vec![
            workspace.join("CLAUDE.md"),
            workspace.join("AGENTS.md"),
            workspace.join(".claude").join("CLAUDE.md"),
        ];
        if let Some(dir) = crate::plan_usage::default_dir() {
            files.push(dir.join("CLAUDE.md"));
        }
        let total = tokio::runtime::Builder::new_current_thread()
            .build()
            .map_or(0, |rt| {
                rt.block_on(async {
                    let mut bytes = 0_u64;
                    for f in files {
                        if let Ok(meta) = tokio::fs::metadata(&f).await {
                            bytes += meta.len();
                        }
                    }
                    bytes / 4
                })
            });
        let _ = slint::invoke_from_event_loop(move || {
            with_state(|s| {
                s.turn.memory_tokens = Some(total);
                s.render_parts();
            });
        });
    });
}

fn sv(text: &str) -> SharedString {
    SharedString::from(text)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or_default()
}

/// Seconds since the epoch, for the elapsed times of the subagent panel (relative to `ChatUi.now-s`).
pub(super) fn now_s() -> i32 {
    i32::try_from(now_ms() / 1000).unwrap_or(i32::MAX)
}

impl UiState {
    pub(super) fn ui(&self) -> ChatUi<'_> {
        self.window.global::<ChatUi>()
    }

    pub(super) fn fail(&self, error: &str) {
        self.window.set_error_text(sv(error));
    }

    /// Sends a command whose reply is applied later on the UI thread (`done`), errors go to the banner.
    pub(super) fn request<T: 'static>(
        &self,
        make: impl FnOnce(Reply<T>) -> Command,
        done: impl FnOnce(&mut UiState, T) + 'static,
    ) {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.send(make(reply));
        let spawned = slint::spawn_local(async move {
            match answer.await {
                Ok(Ok(value)) => with_state(|s| done(s, value)),
                Ok(Err(error)) => with_state(|s| s.fail(&error.to_string())),
                Err(_) => with_state(|s| s.fail(&ApiError::ShutDown.to_string())),
            }
        });
        if spawned.is_err() {
            self.fail("event loop unavailable");
        }
    }

    pub(super) fn chat_call<T: 'static>(
        &self,
        make: impl FnOnce(Reply<T>) -> ChatCall,
        done: impl FnOnce(&mut UiState, T) + 'static,
    ) {
        self.request(|reply| Command::Chat(make(reply)), done);
    }

    fn chat_call_quiet<T: 'static>(
        &self,
        make: impl FnOnce(Reply<T>) -> ChatCall,
        done: impl FnOnce(&mut UiState, T) + 'static,
    ) {
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.send(Command::Chat(make(reply)));
        let _ = slint::spawn_local(async move {
            if let Ok(Ok(value)) = answer.await {
                with_state(|s| done(s, value));
            }
        });
    }

    /// Session of an action: the given id, or the open session when it is empty.
    fn target_session(&self, id: &str) -> Option<SessionId> {
        if id.is_empty() {
            self.active.clone()
        } else {
            Some(id.to_string())
        }
    }

    // ---- per-session reset -------------------------------------------------------------------------

    /// A session was opened or left: drops the live thinking row, plan, usage and change plans.
    pub(super) fn reset_turn_view(&mut self) {
        crate::live::clear(&self.window);
        crate::turn_clock::show(self.active.as_deref());
        crate::speed::show(&self.window, self.active.as_deref());
        self.turn.thought = None;
        self.turn.thought_rows.clear();
        self.turn.next_thought = -2;
        self.turn.change_turns.clear();
        let ui = self.ui();
        ui.set_plan(ModelRc::new(VecModel::<PlanRow>::default()));
        ui.set_subagents(ModelRc::new(VecModel::<SubagentRow>::default()));
        ui.set_subagents_running(0);
        ui.set_subagents_waiting(0);
        ui.set_subagents_active(ModelRc::new(VecModel::<SubagentRow>::default()));
        ui.set_background_tasks(ModelRc::new(VecModel::<BackgroundTaskRow>::default()));
        ui.set_background_tasks_running(0);
        ui.set_parent_id(SharedString::new());
        ui.set_parent_title(SharedString::new());
        ui.set_usage_known(false);
        ui.set_worktree_branch(SharedString::new());
        ui.set_worktree_path(SharedString::new());
        ui.set_session_cost(SharedString::new());
        ui.set_last_assistant(-1);
        self.turn.compact_due = false;
        self.turn.compacted = false;
        self.turn.outputs.clear();
        self.turn.last_usage = None;
        self.ui()
            .set_outputs(ModelRc::new(VecModel::<OutputRow>::default()));
    }

    /// After `SessionOpened`: unread cleared, agent commands, change plans of the finished assistant turns.
    pub(super) fn session_opened(&mut self, items: &[(ItemId, Item)], unread: bool) {
        let Some(session) = self.active.clone() else {
            return;
        };
        if unread && self.turn.keep_unread.as_deref() != Some(session.as_str()) {
            self.set_unread(&session, false);
        }
        self.load_commands();
        for (id, item) in items {
            if let Item::Assistant { .. } = item {
                self.plan_changes(*id, ChangeDirection::Undo);
            }
        }
    }

    pub(super) fn note_item(&mut self, id: ItemId, item: &Item) {
        self.finish_thought();
        if let Item::Assistant { .. } = item {
            self.ui().set_last_assistant(item_id(id));
        }
    }

    pub(super) fn turn_ended(&mut self, id: ItemId) {
        self.finish_thought();
        self.auto_compact();
        if self.rows.contains_key(&id) {
            self.plan_changes(id, ChangeDirection::Undo);
        }
    }

    pub(super) fn set_unread(&self, session: &str, unread: bool) {
        let session = session.to_string();
        self.chat_call(
            |reply| ChatCall::SetUnread {
                session,
                unread,
                reply,
            },
            |_, ()| {},
        );
    }
}

/// Wires the `ChatUi` callbacks of Phase 9.2 R8–R18.
pub(super) fn wire(window: &crate::ui::AppWindow) {
    let ui = window.global::<ChatUi>();
    ui.on_toggle_detail(|id| with_state(|s| s.toggle_detail(id)));
    ui.on_toggle_group(|id| with_state(|s| s.toggle_run(id)));
    ui.on_turn_action(|action, id| with_state(|s| s.turn_action(&action, id)));
    ui.on_session_action(|action, id| with_state(|s| s.session_action(&action, &id)));
    ui.on_group_action(|action, id| with_state(|s| s.group_action(&action, &id)));
    ui.on_drop_session(|session, group| with_state(|s| s.drop_session(&session, &group)));
    ui.on_schedule_action(|action, id| with_state(|s| s.schedule_action(&action, &id)));
    ui.on_subagent_action(|action, id| with_state(|s| s.subagent_action(&action, &id)));
    ui.on_stop_background_task(|id| with_state(|s| s.stop_background_task(&id)));
    ui.on_usage_action(|action| match action.as_str() {
        "toggleAutoCompact" => crate::settings::run_command("toggleAutoCompact"),
        "refreshPlan" => crate::plan_usage::refresh(),
        "redeemCodexReset" => crate::plan_usage::redeem_codex_reset(),
        other => crate::chat::command(other),
    });
    ui.on_dialog_submit(|| with_state(UiState::dialog_submit));
    ui.on_deny_feedback(|request| {
        with_state(|s| {
            if let (Some(session), Ok(request)) = (s.active.clone(), u64::try_from(request)) {
                s.open_dialog(DialogTarget::Deny { session, request }, "", "", true);
            }
        });
    });
    window.on_session_pinned(|id| {
        let mut pinned = false;
        with_state(|s| {
            pinned = s.summaries.iter().any(|x| x.id == id.as_str() && x.pinned);
        });
        pinned
    });
}
