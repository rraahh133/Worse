//! Chat and workspace view logic of Phase 9.2 (spec `docs/specs/harness/phase-9-parity.md` §7): command center,
//! find in session, composer popups / chips / long paste / prompt history, navigation history, export, clipboard.
//!
//! Lives in a `thread_local` on the UI thread like `crate::bridge`. Core facts (sessions, transcript, agents) are
//! read from the bridge on demand; nothing here is persisted or sent to the core except through bridge commands.
//! Rule against re-entrancy: callbacks compute an [`Effect`] while borrowing the state and run it after the borrow
//! ends, because effects reach other UI-thread owners (bridge, settings) that may call back into this module.

mod commands;
mod composer;
mod export;
mod fuzzy;
mod hooks;
mod links;
mod mention;
mod nav;
mod palette;
mod select;
mod whiteboard;
mod wire;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use tokio::sync::mpsc;

use harness_core_api::{ContextItem, MentionCandidate, MentionKind};

use crate::files::{icon_index, FileReq};
use crate::ui::{AppWindow, ChatUi, ChipRow, Labels, PaletteRow};

pub use commands::{command, skill_count};
pub use hooks::{
    add_work_comment, agent_commands, agent_mentions, drop_hover, dropped, exported, files_walked,
    open_path, paste_image, pick_files, prepare_send, restore_composer, save_attachment,
    sessions_changed, set_workspace, toast_label, toast_text,
};
use hooks::{attachments_dir, chip_image, remove_image_chip};
pub use links::open_link;
pub(crate) use links::strip_location_suffix;
pub use wire::install;

/// `ZCode` `LONG_PASTE_TEXT_ATTACHMENT_CHAR_THRESHOLD` (`packages/ui/src/lib/chatAttachments.ts`).
const LONG_PASTE_CHARS: usize = 15 * 1024;
const MAX_ROWS: usize = 50;
const MAX_HISTORY: usize = 100;

/// Command-center sections (`ZCode` `CommandCenter` groups) and their actions, in display order. With an empty query
/// the palette shows "Recent" (actions run from the palette) and then these sections; a query ranks them flat.
const SECTIONS: [(&str, &[&str]); 3] = [
    (
        "suggested",
        &[
            "newTask",
            "findInTask",
            "attachFiles",
            "whiteboard",
            "renameSession",
            "forkSession",
            "pinSession",
            "scheduledPrompts",
            "exportMarkdown",
            "exportHtml",
            "copySessionId",
            "archiveSession",
        ],
    ),
    (
        "panels",
        &[
            "toggleSidePane",
            "switchTheme",
            "previousConversation",
            "nextConversation",
            "navigateBack",
            "navigateForward",
            "zoomIn",
            "zoomOut",
            "resetZoom",
        ],
    ),
    ("configure", &["openSettings", "revealWorkspace"]),
];
/// Most recent palette actions kept for the "Recent" section.
const MAX_RECENT: usize = 5;

/// Local slash commands (agent-advertised commands arrive with the hub's `ChatCall::Commands`, R13).
const SLASH: [(&str, &str); 7] = [
    ("new", "newTask"),
    ("compact", "compactSession"),
    ("autocompact", "toggleAutoCompact"),
    ("find", "findInTask"),
    ("export-md", "exportMarkdown"),
    ("export-html", "exportHtml"),
    ("settings", "openSettings"),
];

#[derive(Clone)]
enum Target {
    Action(String),
    Session(String),
    File(String),
    Agent(String),
    /// Section label row (not runnable).
    Header,
    /// An agent / CLI-config slash command: written into the draft as `/name ` and sent to the agent.
    Slash(String),
    /// Skill or subagent of the active agent (index into `ChatState::skills`, R13).
    Skill(usize),
}

/// Payload of one composer chip (same order as `ChatState::chips`).
#[derive(Clone)]
enum Chip {
    /// Workspace-relative or absolute path; sent as an ACP resource (`ContextItem::File`).
    File(String),
    /// Long paste kept out of the input; folded into the message text (the agents read it as the prompt).
    Paste(String),
    /// Image file (picked, dropped or pasted and saved as PNG).
    Image { path: String, mime: String },
    /// `@session` mention.
    Session(String),
    /// Skill of the active agent: its `SKILL.md` as an ACP resource (R13).
    Skill(ContextItem),
    /// Comment pinned to a Work item element path (`work/<slug>#...`).
    Work { path: String, comment: String },
}

/// File extension → image MIME type of the formats the agents accept.
fn image_mime(path: &str) -> Option<&'static str> {
    let ext = Path::new(path)
        .extension()?
        .to_string_lossy()
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Work that must run after the state borrow ends.
enum Effect {
    None,
    Command(String),
    OpenSession(String),
    ChooseAgent(String),
    /// Session / schedule actions of the bridge and the file picker (run after the chat borrow ends).
    Later(String),
}

struct ChatState {
    window: AppWindow,
    files_tx: mpsc::UnboundedSender<FileReq>,
    workspace: PathBuf,
    exports: PathBuf,
    /// Workspace-relative files (walk result); `None` until the first walk answered.
    files: Option<Vec<String>>,
    walk_sent: bool,
    palette_rows: Rc<VecModel<PaletteRow>>,
    palette_targets: Vec<Target>,
    palette_query: (String, String),
    /// Action ids run from the palette, newest first (command-center history).
    recent: Vec<String>,
    mention_rows: Rc<VecModel<PaletteRow>>,
    mention_targets: Vec<Target>,
    /// Byte offset where the current `@` / `/` token starts in the draft.
    mention_start: usize,
    chips: Rc<VecModel<ChipRow>>,
    /// Chip payloads, same order as `chips`.
    chip_data: Vec<Chip>,
    /// Slash commands of the open session's agent (`ChatCall::Commands`): (name, description, hint).
    agent_commands: Vec<(String, String)>,
    /// Skills and subagents of the open session's agent (`ChatCall::Mentions`, R13).
    skills: Vec<MentionCandidate>,
    /// Pasted images and whiteboard drawings are saved here (`<data>/attachments`).
    attachments: PathBuf,
    last_draft: String,
    history: Vec<String>,
    /// Index into `history` while browsing with ↑/↓, `None` when editing a fresh draft.
    history_pos: Option<usize>,
    history_stash: String,
    /// Sidebar order and the open session (pushed by the bridge).
    sessions: Vec<(String, String)>,
    active: Option<String>,
    nav: Vec<String>,
    nav_pos: usize,
    nav_jump: Option<String>,
    find_query: String,
    find_hits: Vec<(i32, usize)>,
    find_pos: usize,
    toast_timer: slint::Timer,
}

thread_local! {
    static STATE: RefCell<Option<ChatState>> = const { RefCell::new(None) };
}

fn with_state<T>(f: impl FnOnce(&mut ChatState) -> T) -> Option<T> {
    STATE.with(|cell| {
        cell.try_borrow_mut()
            .ok()
            .and_then(|mut guard| guard.as_mut().map(f))
    })
}

fn sv(s: &str) -> SharedString {
    SharedString::from(s)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Timeline group and relative time of a session row (`ZCode` task list). Groups use elapsed time, not local
/// calendar days: std has no time-zone database (documented gap in `uiB-9.2.md`).
pub fn timeline(updated_ms: u64) -> (&'static str, &'static str, i32) {
    let elapsed = now_ms().saturating_sub(updated_ms) / 1000;
    let minutes = elapsed / 60;
    let hours = minutes / 60;
    let days = hours / 24;
    let group = match days {
        0 => "today",
        1 => "yesterday",
        2..=6 => "week",
        7..=29 => "month",
        _ => "older",
    };
    let n = |v: u64| i32::try_from(v).unwrap_or(i32::MAX);
    let (unit, amount) = if minutes == 0 {
        ("now", 0)
    } else if hours == 0 {
        ("m", n(minutes))
    } else if days == 0 {
        ("h", n(hours))
    } else if days < 7 {
        ("d", n(days))
    } else {
        ("w", n(days / 7))
    };
    (group, unit, amount)
}

fn file_row(path: &str) -> PaletteRow {
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
    PaletteRow {
        kind: sv("file"),
        id: sv(path),
        title: sv(name),
        detail: sv(dir),
        shortcut: SharedString::new(),
        icon: icon_index(name, false),
    }
}

impl ChatState {
    fn ui(&self) -> ChatUi<'_> {
        self.window.global::<ChatUi>()
    }

    fn want_files(&mut self) {
        if !self.walk_sent {
            self.walk_sent = true;
            let _ = self.files_tx.send(FileReq::Walk(self.workspace.clone()));
        }
    }

    fn action_title(&self, id: &str) -> String {
        let label = self.window.global::<Labels>().invoke_command_label(sv(id));
        if label.as_str() == id {
            self.ui().invoke_action_label(sv(id)).to_string()
        } else {
            label.to_string()
        }
    }

    fn action_enabled(&self, id: &str) -> bool {
        let has = self.active.is_some();
        match id {
            "exportMarkdown" | "exportHtml" | "findInTask" => {
                self.window.get_chat().row_count() > 0
            }
            "copySessionId" | "archiveSession" | "renameSession" | "forkSession" | "pinSession" => {
                has
            }
            "navigateBack" => self.nav_pos > 0,
            "navigateForward" => self.nav_pos + 1 < self.nav.len(),
            "previousConversation" | "nextConversation" => !self.sessions.is_empty(),
            _ => true,
        }
    }
}

/// Runs an effect outside every state borrow.
fn run(effect: Effect) {
    match effect {
        Effect::None => {}
        Effect::Command(id) => crate::settings::run_command(&id),
        Effect::OpenSession(id) => crate::bridge::open_session(&id),
        Effect::ChooseAgent(id) => crate::bridge::choose_agent(&id),
        Effect::Later(id) => match id.as_str() {
            "attachFiles" => pick_files(),
            "whiteboard" => whiteboard::open(-1),
            "scheduledPrompts" => crate::bridge::schedule_action("open", ""),
            "renameSession" => crate::bridge::session_action("rename", ""),
            "forkSession" => crate::bridge::session_action("fork", ""),
            "pinSession" => crate::bridge::session_action("toggle-pin", ""),
            _ => {}
        },
    }
}

/// Drops the files sender so the core thread can finish.
pub fn teardown() {
    STATE.with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            guard.take();
        }
    });
}
