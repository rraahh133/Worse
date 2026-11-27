//! `State` core: install/teardown, worker requests, status line, snapshots, page switching, reset.

use super::entry::concept_of;
use super::util::parse_hex;
use super::{
    agy, model, mpsc, snap, sv, values, with_state, After, AppWindow, Arc, ComponentHandle,
    ConfigUi, DocUi, HashMap, HashSet, Keymap, Language, NavEntry, Path, PathBuf, ProjectsUi, Req,
    Settings, SettingsUi, SharedString, State, Theme, Value, KEYMAP, NAV, ROW_PAGE, SNAP, STATE,
};

/// Bundle for the stored language; `system` follows the OS locale.
pub(super) fn bundle_for(language: Language) -> &'static str {
    let zh = match language {
        Language::ZhCn => true,
        Language::EnUs => false,
        Language::System => {
            sys_locale::get_locale().is_some_and(|l| l.to_ascii_lowercase().starts_with("zh"))
        }
    };
    if zh {
        "zh_CN"
    } else {
        ""
    }
}

impl State {
    pub(super) fn send(&self, req: Req) {
        // worker 已退出只会发生在关停阶段。
        let _ = self.req.send(req);
    }

    pub(super) fn status(&self, text: &str, error: bool) {
        let ui = self.window.global::<SettingsUi>();
        ui.set_status(sv(text));
        ui.set_status_error(error);
    }

    pub(super) fn set_pending_dir(&self, dir: &str) {
        self.window
            .global::<SettingsUi>()
            .set_pending_data_dir(sv(dir));
    }

    pub(super) fn apply_snapshot(&mut self, settings: Arc<Settings>) {
        let json = serde_json::to_value(&*settings).unwrap_or(Value::Null);
        SNAP.with(|cell| *cell.borrow_mut() = json);
        KEYMAP.with(|cell| *cell.borrow_mut() = Some(Keymap::new(&settings.shortcuts)));
        let theme = self.window.global::<Theme>();
        let pref = match settings.appearance.theme {
            harness_settings::ThemePref::System => 0,
            harness_settings::ThemePref::Light => 1,
            harness_settings::ThemePref::Dark => 2,
        };
        theme.set_pref(pref);
        let zoom = f32::from(settings.appearance.zoom_percent) / 100.0;
        theme.set_ui_font_size(f32::from(settings.appearance.ui_font_size) * zoom);
        theme.set_code_size(f32::from(settings.appearance.code_font_size) * zoom);
        theme.set_reduce_motion(settings.appearance.reduce_motion);
        // "#7C8CFF" was the old default accent and is no longer offered: treat it as "use the default".
        let accent = if settings
            .appearance
            .accent_color
            .eq_ignore_ascii_case("#7C8CFF")
        {
            "#C8F04A"
        } else {
            settings.appearance.accent_color.as_str()
        };
        if let Some(c) = parse_hex(accent) {
            theme.set_accent_pick(c);
        }
        let look = self.window.global::<crate::ui::Look>();
        look.set_thinking(settings.appearance.thinking_style.as_str().into());
        look.set_words(settings.appearance.status_words.as_str().into());
        crate::wallpaper::apply(&self.window, &settings.appearance);
        sync_launch_at_startup(settings.general.launch_at_startup);
        let bundle = bundle_for(settings.general.language);
        if slint::select_bundled_translation(bundle).is_err() {
            self.status("language unavailable", true);
        }
        self.window.set_ui_font_family(
            crate::bridge::ui_font_family(bundle == "zh_CN", &settings.appearance.ui_font).into(),
        );
        self.window
            .global::<crate::ui::ChatUi>()
            .set_auto_compact(settings.general.auto_compact);
        self.window
            .global::<crate::ui::ChatUi>()
            .set_show_speed(settings.appearance.show_token_speed);
        slint::Timer::single_shot(
            std::time::Duration::ZERO,
            crate::bridge::refresh_usage_parts,
        );
        self.settings = settings;
        self.rev = self.rev.wrapping_add(1);
        self.window.global::<SettingsUi>().set_rev(self.rev);
        self.refresh_shortcuts();
        self.refresh_cards();
    }

    pub(super) fn patch(&self, path: &str, value: Value) {
        self.send(Req::Patch {
            path: path.to_string(),
            value,
        });
    }

    pub(super) fn open_page(&mut self, id: &str) {
        let ui = self.window.global::<SettingsUi>();
        ui.set_page(sv(id));
        ui.set_narrow_nav(false);
        ui.set_status(SharedString::new());
        match id {
            "agents" => {
                self.send(Req::Agents(
                    self.agents.iter().map(|a| a.0.clone()).collect(),
                ));
                crate::bridge::request_agent_options();
                crate::bridge::send(crate::core::Command::AgentsOverview);
                crate::bridge::send(crate::core::Command::Accounts(
                    crate::core::AccountsCall::List,
                ));
                crate::bridge::send(crate::core::Command::Providers(
                    crate::core::ProvidersCall::List,
                ));
                agy::refresh();
                self.refresh_cards();
            }
            "orchestra" => crate::orchestra::open(),
            "inbox" => crate::inbox::open(),
            "import" => crate::import::open(),
            "remote" => crate::remote::open(),
            "shortcuts" => self.refresh_shortcuts(),
            "instructions" | "memory" => {
                let doc = self.window.global::<DocUi>();
                doc.set_kind(sv(id));
                doc.set_query(SharedString::new());
                doc.set_matches(model(Vec::new()));
                self.load_doc();
            }
            "projects" => {
                self.selected.clear();
                self.removed.clear();
                self.window.global::<ProjectsUi>().set_undo_count(0);
                self.refresh_projects();
            }
            "archive" => self.refresh_archive(0),
            "plugins" => crate::plugins::open(),
            concept if concept_of(concept).is_some() => {
                let cfg = self.window.global::<ConfigUi>();
                cfg.set_concept(sv(concept));
                cfg.set_limit(ROW_PAGE);
                if cfg.get_agent() == "harness" && concept != "mcp" && concept != "skill" {
                    cfg.set_agent(sv("claude-code"));
                }
                cfg.set_query(SharedString::new());
                self.after = After::Config;
                self.close_editor();
                self.reload_config();
                if concept == "skill" {
                    crate::skills::open();
                }
            }
            _ => {}
        }
    }
}

impl State {
    pub(super) fn page(&self) -> SharedString {
        self.window.global::<SettingsUi>().get_page()
    }
}

/// Settings S1 "Launch at startup": mirrors the flag into HKCU\…\Run (Windows), only when it changes.
pub(super) fn sync_launch_at_startup(on: bool) {
    thread_local!(static LAST: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) });
    let prev = LAST.with(|c| c.replace(Some(on)));
    // First snapshot after start: only write when enabling (keeps the entry pointing at the current build).
    if prev == Some(on) || (prev.is_none() && !on) {
        return;
    }
    #[cfg(windows)]
    {
        let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
        let mut cmd = std::process::Command::new("reg");
        if on {
            let exe = crate::live_update::launch_exe();
            cmd.args(["add", key, "/v", "Harness", "/t", "REG_SZ", "/f", "/d"])
                .arg(format!("\"{}\"", exe.display()));
        } else {
            cmd.args(["delete", key, "/v", "Harness", "/f"]);
        }
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        std::thread::spawn(move || {
            let _ = cmd.output();
        });
    }
}

/// Settings S1 "Reset all settings to defaults": app preferences only — projects, sessions, agent prefs,
/// remote access and API keys are kept.
pub(super) fn reset_settings(s: &State) {
    let defaults = serde_json::to_value(Settings::default()).unwrap_or(Value::Null);
    for section in [
        "general",
        "appearance",
        "notifications",
        "updates",
        "orchestra",
        "security",
    ] {
        if let Some(v) = defaults.get(section) {
            let mut v = v.clone();
            // Keep the last workspace so the next start reopens the same project.
            if section == "general" {
                if let (Some(obj), Some(last)) =
                    (v.as_object_mut(), s.settings.general.last_workspace.clone())
                {
                    obj.insert("lastWorkspace".into(), Value::from(last));
                }
            }
            s.patch(section, v);
        }
    }
    s.status("settings reset to defaults", false);
}

/// Installs the settings state and callbacks. `settings` is the store's snapshot at startup, so the first frame
/// already has the stored theme, language and zoom.
pub fn install(
    window: &AppWindow,
    req: mpsc::UnboundedSender<Req>,
    settings: Arc<Settings>,
    data_dir: &Path,
) {
    let ui = window.global::<SettingsUi>();
    ui.set_nav(model(
        NAV.iter()
            .map(|(id, group)| NavEntry {
                id: sv(id),
                group: *group,
            })
            .collect(),
    ));
    ui.set_data_dir(sv(data_dir.display().to_string()));
    let state = State {
        window: window.clone_strong(),
        req,
        settings: Arc::new(Settings::default()),
        rev: 0,
        seq: 0,
        agents: Vec::new(),
        auth: HashMap::new(),
        launch: HashMap::new(),
        options: HashMap::new(),
        support: None,
        entries: Vec::new(),
        edit: None,
        doc_original: String::new(),
        doc_path: PathBuf::new(),
        after: After::Config,
        overview: HashMap::new(),
        adapter_missing: HashSet::new(),
        accounts: Vec::new(),
        providers: Vec::new(),
        tests: HashMap::new(),
        list_seq: 0,
        projects: Vec::new(),
        selected: HashSet::new(),
        removed: Vec::new(),
        archive: Vec::new(),
    };
    STATE.with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            *guard = Some(state);
        }
    });
    with_state(|s| s.apply_snapshot(settings));
    super::wire(window);
}

#[allow(clippy::too_many_lines)]
pub(super) fn wire(window: &AppWindow) {
    let ui = window.global::<SettingsUi>();
    ui.on_get_bool(|key, _| {
        snap(|v| {
            values::get(v, &key)
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
    });
    ui.on_get_int(|key, _| {
        snap(|v| match values::get(v, &key) {
            Some(Value::String(s)) if key == "appearance.theme" => match s.as_str() {
                "light" => 1,
                "dark" => 2,
                _ => 0,
            },
            Some(value) => value
                .as_i64()
                .and_then(|n| i32::try_from(n).ok())
                .unwrap_or(0),
            None => 0,
        })
    });
    ui.on_get_string(|key, _| {
        // Read-only app facts for the settings nav footer (not stored in settings.json).
        if key == "app.version" {
            return SharedString::from(env!("CARGO_PKG_VERSION"));
        }
        snap(|v| match values::get(v, &key) {
            Some(Value::String(s)) => sv(s),
            Some(Value::Number(n)) => sv(n.to_string()),
            _ => SharedString::new(),
        })
    });
    ui.on_set_bool(|key, on| with_state(|s| s.patch(&key, Value::from(on))));
    ui.on_set_int(|key, n| with_state(|s| s.patch(&key, Value::from(n))));
    ui.on_set_string(|key, text| with_state(|s| s.patch(&key, Value::from(text.as_str()))));
    ui.on_open_page(|id| with_state(|s| s.open_page(&id)));
    ui.on_reset_settings(|| with_state(|s| reset_settings(s)));
    ui.on_relocate_data_dir(|target| {
        with_state(|s| {
            let path = PathBuf::from(target.trim());
            if path.is_absolute() {
                s.send(Req::Relocate(path));
            } else {
                s.status("data directory must be an absolute path", true);
            }
        });
    });
    ui.on_cancel_relocation(|| with_state(|s| s.send(Req::CancelRelocation)));
}

/// The last settings snapshot mirrored from the store (UI thread only).
pub fn current() -> Option<Arc<Settings>> {
    let mut out = None;
    with_state(|s| out = Some(Arc::clone(&s.settings)));
    out
}

/// Drops the settings state (window handle and worker sender) so the worker can end.
pub fn teardown() {
    STATE.with(|cell| {
        if let Ok(mut guard) = cell.try_borrow_mut() {
            guard.take();
        }
    });
}
