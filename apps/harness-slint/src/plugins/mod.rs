//! Plugin store page (`docs/plans/plugins.md` §2): lists the configured marketplaces, reads plugins, and asks
//! the settings worker for install / uninstall plans. This module never writes an agent config file or the
//! installed record: every write goes through `Req::Plan` (diff dialog) or `Req::PluginBuiltin`.

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Instant;

use harness_plugins::store::MarketplaceSource;
use harness_plugins::{Installed, Marketplace, Plugin};
use slint::ComponentHandle;
use tokio::sync::mpsc::UnboundedSender;

use crate::settings::{PlanOp, Req};
use crate::ui::{AppWindow, PluginsUi};

mod actions;
mod connector_rows;
mod connectors;
mod icons;
mod load;
mod rows;

/// Discover rows shown before "show more".
const PAGE: usize = 30;
/// Key prefix of the plugins shipped inside Harness.
const BUILTIN: &str = "builtin";
/// Built-in plugins: (server, title, description).
const BUILTINS: [(&str, &str, &str); 2] = [
    (
        harness_agent_config::overlay::WORK_SERVER,
        "Office documents",
        "Create and edit Word, Excel and PowerPoint files (Harness Work) from any agent.",
    ),
    (
        harness_agent_config::overlay::COMPUTER_SERVER,
        "Computer use",
        "Lets agents take screenshots and drive the mouse and keyboard on this machine.",
    ),
];

struct Market {
    source: MarketplaceSource,
    dir: PathBuf,
    data: Option<Marketplace>,
    error: String,
}

struct State {
    window: slint::Weak<AppWindow>,
    req: UnboundedSender<Req>,
    data_dir: PathBuf,
    markets: Vec<Market>,
    installed: Installed,
    disabled_builtins: Vec<String>,
    /// "connectors" | "plugins".
    section: String,
    connectors: connectors::Connectors,
    tab: String,
    query: String,
    category: String,
    limit: usize,
    detail_key: Option<String>,
    detail: Option<Plugin>,
    detail_loading: bool,
    busy: Option<String>,
    loading: bool,
    status: String,
    status_error: bool,
    last_fetch: Option<Instant>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|cell| cell.try_borrow_mut().ok()?.as_mut().map(f))
}

pub fn install(window: &AppWindow, req: UnboundedSender<Req>, data_dir: &std::path::Path) {
    STATE.with(|cell| {
        *cell.borrow_mut() = Some(State {
            window: window.as_weak(),
            req,
            data_dir: data_dir.to_path_buf(),
            markets: Vec::new(),
            installed: Installed::default(),
            disabled_builtins: Vec::new(),
            section: "connectors".to_owned(),
            connectors: connectors::Connectors::new(),
            tab: String::new(),
            query: String::new(),
            category: String::new(),
            limit: PAGE,
            detail_key: None,
            detail: None,
            detail_loading: false,
            busy: None,
            loading: false,
            status: String::new(),
            status_error: false,
            last_fetch: None,
        });
    });
    let ui = window.global::<PluginsUi>();
    ui.on_search(|q| {
        with(|s| {
            s.query = q.to_string();
            s.limit = PAGE;
            s.connectors.limit = PAGE;
        });
        rows::push();
    });
    ui.on_set_tab(|tab| {
        with(|s| {
            s.tab = tab.to_string();
            s.limit = PAGE;
            s.connectors.limit = PAGE;
            s.detail_key = None;
            s.detail = None;
        });
        rows::push();
    });
    ui.on_set_category(|c| {
        with(|s| {
            s.category = c.to_string();
            s.limit = PAGE;
            s.connectors.limit = PAGE;
        });
        rows::push();
    });
    ui.on_show_more(|| {
        // 「显示全部」：一次放开当前分区的全部条目。
        with(|s| {
            if s.section == "connectors" {
                s.connectors.limit = usize::MAX;
            } else {
                s.limit = usize::MAX;
            }
        });
        rows::push();
    });
    ui.on_set_section(|section| {
        with(|s| {
            s.section = section.to_string();
            s.category.clear();
            s.detail_key = None;
            s.detail = None;
            if s.tab == "marketplaces" && s.section == "connectors" {
                s.tab.clear();
            }
        });
        rows::push();
    });
    ui.on_connect(|id| connectors::connect(&id, None));
    ui.on_disconnect(|id| connectors::disconnect(&id));
    ui.on_set_connector_enabled(|id, on| connectors::set_enabled(&id, on));
    ui.on_submit_client(|id, client, secret| connectors::submit_client(&id, &client, &secret));
    ui.on_cancel_client(connectors::cancel_client);
    ui.on_add_connector(|name, url| connectors::add_custom(&name, &url));
    ui.on_refresh(|| load::spawn_markets(true));
    ui.on_open_detail(|key| actions::open_detail(&key));
    ui.on_close_detail(|| {
        with(|s| {
            s.detail_key = None;
            s.detail = None;
            s.detail_loading = false;
        });
        rows::push();
    });
    ui.on_install(|key| actions::install(&key));
    ui.on_uninstall(|key| actions::uninstall(&key));
    ui.on_set_enabled(|key, on| actions::set_enabled(&key, on));
    ui.on_add_market(|source| actions::add_market(&source));
    ui.on_remove_market(|source| actions::remove_market(&source));
    ui.on_open_url(|url| crate::remote::open_url(&url));
}

/// The Plugins page was opened: show what is cached, refresh marketplaces at most once a day.
pub fn open() {
    rows::push();
    load::spawn_markets(false);
    connectors::spawn_state();
}

/// The worker finished (or the user cancelled) a plugin write; `Err("")` = cancelled.
pub fn finished(result: Result<(), String>) {
    with(|s| {
        s.busy = None;
        match result {
            Ok(()) => {
                "Done. New agent sessions pick up the change.".clone_into(&mut s.status);
                s.status_error = false;
            }
            Err(e) if e.is_empty() => s.status.clear(),
            Err(e) => {
                s.status = e;
                s.status_error = true;
            }
        }
    });
    load::spawn_state();
}

/// Sends a plan request; the diff dialog opens when the worker answers.
fn plan(title: &'static str, op: PlanOp) {
    let sent = with(|s| s.req.send(Req::Plan { title, op }).is_ok()).unwrap_or(false);
    if !sent {
        fail("the settings worker is not running".to_owned());
    }
}

/// A neutral status line ("" clears it).
fn status(text: &str) {
    with(|s| {
        text.clone_into(&mut s.status);
        s.status_error = false;
    });
    rows::push();
}

fn fail(error: String) {
    with(|s| {
        s.busy = None;
        s.status = error;
        s.status_error = true;
    });
    rows::push();
}
