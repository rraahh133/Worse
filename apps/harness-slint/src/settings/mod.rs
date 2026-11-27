//! Settings pages, UI-thread side (spec phase-9-parity.md §6.3). State owners:
//! - `settings.json`: the shared `SettingsStore` (one per process, opened in `main`); this module only sends
//!   patches to the worker and mirrors the store's watch snapshots (`apply_snapshot`).
//! - Agent CLI files: the worker plans, the user confirms the diff, the worker applies.
//! - MEMORY.md, projects, archive: the hub (through `Command::Settings`); the worker only forwards the calls.
//! - Agents overview / connection test: the hub (`Command::AgentsOverview` / `TestAgent`, answered as events).
//! - Page-local state (form fields, query, recorder) lives here and is not persisted.
//!
//! This file is wiring only: the shared `State`, the crate-visible entry points and the helpers every page
//! uses. Each page is a submodule holding its own `impl State` block and its own Slint callbacks (`wire`).

mod accounts;
mod agents;
pub mod agy;
mod archive;
mod config;
pub mod decision;
mod diff;
mod doc;
mod docview;
mod entry;
mod form;
pub mod graph;
mod graph_view;
pub mod keymap;
mod presets;
mod projects;
pub mod recall;
mod shortcuts;
mod state;
#[cfg(test)]
mod tests;
mod util;
mod values;
mod worker;

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use harness_agent_config::{
    AgentKind, Command as CmdEntry, Concept, ConceptSupport, ConfigTarget, Entry, EntryBody, Hook,
    McpServer, McpTransport, Scope, Skill, Subagent,
};
use harness_core_api as api;
use harness_settings::{Language, Settings};
use serde_json::Value;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use tokio::sync::mpsc;

use crate::ui::{
    AccountRow, AgentCard, AgentsUi, AppWindow, ArchiveRow, ArchiveUi, ConfigUi, DiffLine, DiffUi,
    DocUi, FormField, Labels, NavEntry, ProjectRow, ProjectsUi, ProviderRow, ResGroup, ResRow,
    SelectOption, SettingsUi, ShortcutRow, ShortcutsUi, Theme,
};
use keymap::{Keymap, Press, Verdict};
use worker::DocKind;
pub use worker::{PlanOp, Req, Worker};

const NAV: &[(&str, i32)] = &[
    ("general", 0),
    ("appearance", 0),
    ("agents", 0),
    ("orchestra", 0),
    ("usage", 0),
    ("inbox", 0),
    ("remote", 0),
    ("security", 0),
    ("shortcuts", 0),
    ("instructions", 1),
    ("memory", 1),
    ("subagent", 1),
    ("mcp", 1),
    ("skill", 1),
    ("command", 1),
    ("plugins", 1),
    // Hooks page stays built but is hidden from the nav (user request); add ("hook", 1) back to show it.
    ("projects", 2),
    ("archive", 2),
    ("import", 2),
    ("performance", 2),
];

const ROW_PAGE: i32 = 10;

/// A loaded instructions / memory document.
pub struct Doc {
    pub path: PathBuf,
    pub exists: bool,
    pub text: String,
    pub supported: bool,
    pub reason: String,
}

struct Edit {
    previous: Option<String>,
    base: Option<EntryBody>,
    fields: Vec<(String, String)>,
}

#[derive(Default)]
struct AgentOptions {
    modes: Vec<(String, String)>,
    models: Vec<(String, String)>,
    efforts: Vec<(String, String)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum After {
    Config,
    Doc,
}

pub struct State {
    window: AppWindow,
    req: mpsc::UnboundedSender<Req>,
    settings: Arc<Settings>,
    rev: i32,
    seq: u64,
    agents: Vec<(String, String, bool)>,
    auth: HashMap<String, i32>,
    launch: HashMap<String, (String, String, String)>,
    options: HashMap<String, AgentOptions>,
    support: Option<ConceptSupport>,
    entries: Vec<Entry>,
    edit: Option<Edit>,
    doc_original: String,
    doc_path: PathBuf,
    after: After,
    /// Hub overview per agent: (version, auth 0 present / 1 absent / 2 unknown).
    overview: HashMap<String, (String, i32)>,
    /// Agents whose CLI is installed but whose ACP adapter is missing (hub overview, §9.10).
    adapter_missing: HashSet<String>,
    /// Extra Claude accounts (hub, phase-9-parity.md §9.6).
    accounts: Vec<api::AccountInfo>,
    /// Custom API providers (hub, phase-9-parity.md §9.7; never with the key).
    providers: Vec<api::ProviderInfo>,
    /// Connection test per agent: (state 1 running / 2 ok / 3 failed, latency ms, error).
    tests: HashMap<String, (i32, i32, String)>,
    /// Sequence of the projects / archive lists (separate from the config / document `seq`).
    list_seq: u64,
    projects: Vec<api::ProjectInfo>,
    selected: HashSet<String>,
    /// Keys removed by the last remove (undo restores them).
    removed: Vec<String>,
    archive: Vec<ArchiveRow>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    // Pure-callback reads (bindings) never borrow STATE: they use this copy of the snapshot and the keymap.
    static SNAP: RefCell<Value> = const { RefCell::new(Value::Null) };
    static KEYMAP: RefCell<Option<Keymap>> = const { RefCell::new(None) };
}

pub(super) fn with_state(f: impl FnOnce(&mut State)) {
    STATE.with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            if let Some(state) = guard.as_mut() {
                f(state);
            }
        }
    });
}

/// Saves one setting changed on a phone (callable from any thread). The value is checked against a copy
/// of `current` first, so a bad value is reported to the phone instead of only on the desktop.
///
/// # Errors
/// The value does not fit the setting, or the UI event loop is gone.
pub(crate) fn patch_remote(
    current: &harness_settings::Settings,
    path: &str,
    value: Value,
) -> Result<(), String> {
    let mut probe = current.clone();
    values::set_path(&mut probe, path, value.clone())?;
    let path = path.to_owned();
    slint::invoke_from_event_loop(move || with_state(|s| s.patch(&path, value)))
        .map_err(|e| e.to_string())
}

/// Stored value at a dotted settings path (remote row tests).
#[cfg(test)]
pub(crate) fn value_at(settings: &harness_settings::Settings, path: &str) -> Option<Value> {
    let root = serde_json::to_value(settings).ok()?;
    values::get(&root, path).cloned()
}

fn snap<T>(f: impl FnOnce(&Value) -> T) -> T {
    SNAP.with(|cell| f(&cell.borrow()))
}

fn keymap<T>(f: impl FnOnce(&Keymap) -> T) -> Option<T> {
    KEYMAP.with(|cell| cell.borrow().as_ref().map(f))
}

fn sv(s: impl AsRef<str>) -> SharedString {
    SharedString::from(s.as_ref())
}

fn model<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
    ModelRc::from(Rc::new(VecModel::from(items)))
}

/// Writes one setting through the settings worker (the store's single write path), e.g. `setup.firstRunDone`.
pub fn patch_setting(path: &str, value: Value) {
    with_state(|s| s.patch(path, value));
}

pub use accounts::{accounts, providers};
pub use agents::{agent_options, agent_tested, agents_changed, agents_overview, programs_applied};
pub(crate) use diff::diff_lines;
pub use projects::sessions_changed;
pub use shortcuts::{run_command, shortcut_label};
pub use state::{current, install, teardown};
pub(crate) use util::date_of;

/// Registers the Slint callbacks of every settings page, in the order the globals appear.
fn wire(window: &AppWindow) {
    state::wire(window);
    shortcuts::wire(window);
    agents::wire(window);
    accounts::wire(window);
    agy::wire(window);
    presets::install(window);
    config::wire(window);
    form::wire(window);
    diff::wire(window);
    doc::wire(window);
    projects::wire(window);
    archive::wire(window);
}
