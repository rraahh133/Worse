//! Typed settings (spec §9.1.1). Every field has a serde default, so a missing or partial file yields defaults.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

mod sections;

pub use sections::{
    AgentPrefs, MemorySettings, NotificationSettings, OrchestraSettings, RemoteSettings,
    SecuritySettings, SetupSettings, UpdateSettings,
};

/// Schema version this build reads and writes.
pub const SCHEMA_VERSION: u64 = 1;

/// Root of `settings.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub schema_version: u64,
    pub general: GeneralSettings,
    pub appearance: AppearanceSettings,
    pub notifications: NotificationSettings,
    pub updates: UpdateSettings,
    /// Keyboard shortcut overrides: command id → bindings (verbatim strings). Defaults live in the UI keymap.
    pub shortcuts: BTreeMap<String, Vec<String>>,
    /// Per agent id preferences.
    pub agents: BTreeMap<String, AgentPrefs>,
    pub memory: MemorySettings,
    /// First-run setup (phase-9-parity.md §9.10).
    pub setup: SetupSettings,
    /// Phone access (phase-12-remote-phone.md): local HTTP server + tunnel.
    pub remote: RemoteSettings,
    /// Orchestra page: delegation defaults (settings redesign S3).
    #[serde(default)]
    pub orchestra: OrchestraSettings,
    /// Security page (security-panel.md §5): dependency audit and commit gate.
    #[serde(default)]
    pub security: SecuritySettings,
    #[serde(default)]
    pub research: ResearchSettings,
    /// Skill trigger health check (skill-health.md): the opt-in live check only.
    #[serde(default)]
    pub skill_health: SkillHealthSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ResearchSettings {
    pub fact_check: bool,
}

impl Default for ResearchSettings {
    fn default() -> Self {
        Self { fact_check: true }
    }
}

/// Skill trigger health check (skill-health.md): the live check spends the user's quota, so it is off by default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SkillHealthSettings {
    /// Run skill-reach against a real CLI. Harness only builds the command; the user starts it.
    pub live_check: bool,
    /// skill-reach binary, resolved on `PATH` unless this is an absolute path.
    pub binary: String,
}

impl Default for SkillHealthSettings {
    fn default() -> Self {
        Self {
            live_check: false,
            binary: "reach".into(),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            general: GeneralSettings::default(),
            appearance: AppearanceSettings::default(),
            notifications: NotificationSettings::default(),
            updates: UpdateSettings::default(),
            shortcuts: BTreeMap::new(),
            agents: BTreeMap::new(),
            memory: MemorySettings::default(),
            setup: SetupSettings::default(),
            remote: RemoteSettings::default(),
            orchestra: OrchestraSettings::default(),
            security: SecuritySettings::default(),
            research: ResearchSettings::default(),
            skill_health: SkillHealthSettings::default(),
        }
    }
}

impl Settings {
    /// Copy with every bounded field clamped to its range (applied on load and on every patch).
    #[must_use]
    pub fn normalized(mut self) -> Self {
        let a = &mut self.appearance;
        a.ui_font_size = a.ui_font_size.clamp(12, 20);
        a.code_font_size = a.code_font_size.clamp(10, 24);
        a.zoom_percent = a.zoom_percent.clamp(80, 150);
        let t = &mut self.general.terminal;
        t.font_size = t.font_size.clamp(8, 32);
        self.general.auto_archive_days = self.general.auto_archive_days.clamp(1, 3650);
        self.orchestra.max_concurrent = self.orchestra.max_concurrent.max(1);
        self.orchestra.max_nested_per_subagent = self.orchestra.max_nested_per_subagent.max(1);
        self.orchestra.canvas_zoom = self.orchestra.canvas_zoom.clamp(40, 200);
        // A privileged or zero port cannot be bound by an unprivileged process: fall back to the default.
        if self.remote.port < 1024 {
            self.remote.port = RemoteSettings::default().port;
        }
        self.schema_version = SCHEMA_VERSION;
        self
    }

    /// Preferences of `agent_id` (defaults when not set).
    #[must_use]
    pub fn agent(&self, agent_id: &str) -> AgentPrefs {
        self.agents.get(agent_id).cloned().unwrap_or_default()
    }
}

/// UI language. `System` follows the OS locale.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[default]
    #[serde(rename = "system")]
    System,
    #[serde(rename = "en-US")]
    EnUs,
    #[serde(rename = "zh-CN")]
    ZhCn,
}

impl Language {
    /// Parses a stored tag (`system`, `en-US`, `zh-CN`, case-insensitive; `en`/`zh` prefixes accepted).
    #[must_use]
    pub fn from_tag(tag: &str) -> Option<Self> {
        let lower = tag.trim().to_ascii_lowercase();
        match lower.as_str() {
            "system" | "" => Some(Self::System),
            t if t.starts_with("zh") => Some(Self::ZhCn),
            t if t.starts_with("en") => Some(Self::EnUs),
            _ => None,
        }
    }
}

/// Colour theme.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThemePref {
    #[default]
    System,
    Light,
    Dark,
}

/// Follow-up message while a turn runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FollowUp {
    /// Queue it for after the turn.
    #[default]
    Queue,
    /// Steer the running turn (called guide in the old app).
    #[serde(alias = "guide")]
    Steer,
}

/// Composer send key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ComposerSend {
    #[default]
    Enter,
    ModEnter,
}

/// General page.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GeneralSettings {
    pub language: Language,
    pub terminal: TerminalSettings,
    pub proxy: ProxySettings,
    /// Legacy switch that was never read (it defaulted to on while the app drew on the CPU); kept so old
    /// settings files load. `gpu_rendering` is the real one.
    pub hardware_acceleration: bool,
    /// Draw with the GPU (Skia on OpenGL / Direct3D / Metal) instead of the CPU. Smoother on very large
    /// screens and with animated wallpapers, at about 170 MB more memory. On by default. Takes effect after restart.
    pub gpu_rendering: bool,
    pub follow_up: FollowUp,
    pub composer_send: ComposerSend,
    pub show_reasoning: bool,
    pub show_todos: bool,
    pub group_exploration: bool,
    pub group_terminal: bool,
    pub group_file_changes: bool,
    pub auto_archive: bool,
    pub auto_archive_days: u32,
    pub auto_compact: bool,
    pub last_workspace: Option<String>,
    /// Open Harness when the user signs in (settings S1 "Launch at startup").
    pub launch_at_startup: bool,
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            language: Language::System,
            terminal: TerminalSettings::default(),
            proxy: ProxySettings::default(),
            hardware_acceleration: true,
            gpu_rendering: true,
            follow_up: FollowUp::Queue,
            composer_send: ComposerSend::Enter,
            show_reasoning: true,
            show_todos: true,
            group_exploration: true,
            group_terminal: true,
            group_file_changes: true,
            auto_archive: false,
            auto_archive_days: 30,
            auto_compact: true,
            last_workspace: None,
            launch_at_startup: false,
        }
    }
}

/// Integrated terminal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TerminalSettings {
    pub inherit_system_profile: bool,
    pub font_family: Option<String>,
    pub font_size: u16,
    pub shell: Option<String>,
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            inherit_system_profile: true,
            font_family: None,
            font_size: 13,
            shell: None,
        }
    }
}

/// Network proxy passed to agent CLIs (env) in 9.5.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProxySettings {
    pub http_proxy: Option<String>,
    pub no_proxy: Option<String>,
    pub ca_cert_path: Option<String>,
}

/// Appearance page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct AppearanceSettings {
    pub theme: ThemePref,
    /// "jakarta" | "inter" | "system".
    pub ui_font: String,
    pub ui_font_size: u16,
    pub zoom_percent: u16,
    pub code_theme_light: String,
    pub code_theme_dark: String,
    pub line_numbers: bool,
    pub wrap_lines: bool,
    pub code_font_size: u16,
    /// Use the OS window frame (Windows title bar + native resize / snap) instead of the custom
    /// borderless chrome. Applied at startup: winit cannot switch decorations on a live window.
    pub native_frame: bool,
    /// Accent colour "#RRGGBB" (settings S2 swatches).
    pub accent_color: String,
    /// "compact" | "comfortable".
    pub density: String,
    pub reduce_motion: bool,
    /// Context-use ring next to each session in the sidebar.
    pub show_token_usage: bool,
    /// Unicode animation beside "Thinking…": "scan" | "bars" | "pulse" | "quads" | "charge" | "moon" |
    /// "cipher" | "chevrons" | "bunny" | "shuffle" (a different one every turn).
    pub thinking_style: String,
    /// Words beside it: "plain" ("Thinking…") | "playful" (a rotating set).
    pub status_words: String,
    /// Generation speed ("tok/s") beside the working line and under each finished answer.
    pub show_token_speed: bool,
    /// Unicode field at the window edges: "dither" | "rain" | "stars" | "circuit" | "none".
    pub backdrop_field: String,
    /// When the field moves: "home" (empty chat only) | "always" | "off".
    pub backdrop_motion: String,
    /// Wallpaper behind the window: path of an image or an animated GIF / WebP ("" = none).
    pub wallpaper_path: String,
    /// Blur of the wallpaper, 0..=40 (pixels of a 1440 px wide window).
    pub wallpaper_blur: u16,
    /// How much of the canvas colour lies over the wallpaper, percent 0..=95.
    pub wallpaper_dim: u16,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            theme: ThemePref::System,
            ui_font: "jakarta".into(),
            ui_font_size: 14,
            zoom_percent: 100,
            code_theme_light: "github-light".into(),
            code_theme_dark: "github-dark".into(),
            line_numbers: true,
            wrap_lines: false,
            code_font_size: 13,
            native_frame: true,
            accent_color: "#C8F04A".into(),
            density: "comfortable".into(),
            reduce_motion: false,
            show_token_usage: true,
            thinking_style: "scan".into(),
            status_words: "plain".into(),
            show_token_speed: true,
            backdrop_field: "dither".into(),
            backdrop_motion: "home".into(),
            wallpaper_path: String::new(),
            wallpaper_blur: 8,
            wallpaper_dim: 55,
        }
    }
}

#[cfg(test)]
mod tests;
