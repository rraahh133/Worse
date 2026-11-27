use super::{
    computer_mcp, live_update, release_update, relocation, ui, window_frame, Arc, ExitCode, Path,
    PathBuf, SettingsStore,
};

/// `HARNESS_HOME`, else `~/.harness`.
pub(super) fn harness_home() -> PathBuf {
    if let Some(home) = std::env::var_os("HARNESS_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(home);
    }
    let user_home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    user_home.join(".harness")
}

/// The workspace to open: `HARNESS_WORKSPACE`, else the folder Harness was started in from a
/// terminal, else the last workspace, else the home folder. Like Antigravity, a start from the app
/// menu, a shortcut, Finder or the installer never opens the folder holding the executable.
pub(super) fn workspace(last: Option<&str>) -> PathBuf {
    if let Some(dir) = std::env::var_os("HARNESS_WORKSPACE").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .filter(|p| p.is_dir());
    // The exe that owns the shipped files: the source exe when this is a live-update shadow copy.
    let exe_dir = live_update::launch_exe().parent().map(Path::to_path_buf);
    let same = |a: &Path, b: &Path| match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    let system = std::env::var_os("SystemRoot").map(PathBuf::from);
    // 修复：从开始菜单/快捷方式/安装程序启动时，cwd 是安装目录或 System32，不能当成工作区。
    let not_chosen = cwd.parent().is_none()
        || [exe_dir, crate::install::install_dir(), home.clone()]
            .iter()
            .flatten()
            .any(|dir| same(dir, &cwd))
        || system.is_some_and(|dir| cwd.starts_with(dir));
    if !not_chosen {
        return cwd;
    }
    if let Some(last) = last.map(PathBuf::from).filter(|p| p.is_dir()) {
        return last;
    }
    home.unwrap_or(cwd)
}

pub(super) fn prepare_process() {
    #[cfg(target_os = "macos")]
    fix_macos_path();
    select_renderer();
}

#[cfg(target_os = "macos")]
pub(super) fn login_shell_path() -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    const MARK: &str = "__HARNESS_PATH__";
    let shell = std::env::var_os("SHELL")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/zsh".into());
    let mut child = Command::new(shell)
        .args([
            "-l",
            "-i",
            "-c",
            &format!("printf '{MARK}%s{MARK}' \"$PATH\""),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    out.split(MARK).nth(1).map(str::to_owned)
}

#[cfg(target_os = "macos")]
pub(super) fn fix_macos_path() {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let mut extra: Vec<PathBuf> = login_shell_path()
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    extra.push(PathBuf::from("/opt/homebrew/bin"));
    extra.push(PathBuf::from("/usr/local/bin"));
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        extra.push(PathBuf::from(home).join(".local").join("bin"));
    }
    for dir in extra {
        if !dir.as_os_str().is_empty() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    if let Ok(joined) = std::env::join_paths(&dirs) {
        std::env::set_var("PATH", joined);
    }
}

pub(super) static RELAUNCH: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

pub(super) fn install_restart_for_update(window: &ui::AppWindow, workspace: &Path) {
    let dir = workspace.to_path_buf();
    window.on_restart_for_update(move || {
        if let Ok(mut slot) = RELAUNCH.lock() {
            *slot = Some(dir.clone());
        }
        let _ = slint::quit_event_loop();
    });
}

pub(super) fn relaunch() {
    let Some(dir) = RELAUNCH.lock().ok().and_then(|mut slot| slot.take()) else {
        // 同 Antigravity：已下载更新时，直接退出也会运行安装程序（装完自动重新打开）。
        let dir = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        crate::install::run_pending(&dir);
        return;
    };
    // A downloaded installer (Windows update / first install) or the freshly installed copy.
    if crate::install::run_pending(&dir) {
        return;
    }
    // A shadow copy restarts the newest build (its source exe), not itself.
    let exe = release_update::installed_exe().unwrap_or_else(live_update::launch_exe);
    if let Err(error) = std::process::Command::new(exe)
        .current_dir(&dir)
        .env("HARNESS_WORKSPACE", &dir)
        .spawn()
    {
        eprintln!(
            "harness-slint: cannot restart in {}: {error}",
            dir.display()
        );
    }
}

/// Renderer choice (phase-10 Midnight Glass UI §2). Default: Skia rasterizing on the CPU (`skia-software`).
/// It draws exactly what GPU Skia draws (same Skia, same glyph hinting), but repaints only the dirty region
/// (GPU Skia redraws the whole window for every change) and needs no GPU context, which saves memory.
/// `HARNESS_RENDERER=skia` (GPU Skia first) suits very large or high-refresh screens, where scrolling the
/// whole window on the CPU costs more. `femtovg|software` put those first; `SLINT_BACKEND` overrides all.
pub(super) fn select_renderer() {
    if std::env::var_os("SLINT_BACKEND").is_some_and(|v| !v.is_empty()) {
        return;
    }
    let order: &[&str] = match std::env::var("HARNESS_RENDERER").as_deref() {
        Ok("software") => &["software", "skia-software", "skia", "femtovg"],
        Ok("femtovg") => &["femtovg", "skia-software", "skia", "software"],
        Ok("skia" | "gpu") => &["skia", "skia-software", "femtovg", "software"],
        Ok("skia-software" | "cpu") => &["skia-software", "skia", "femtovg", "software"],
        // No override: Settings › General › GPU rendering decides (on by default).
        _ if gpu_rendering_setting(&harness_home()) => {
            &["skia", "skia-software", "femtovg", "software"]
        }
        _ => &["skia-software", "skia", "femtovg", "software"],
    };
    for &renderer in order {
        match slint::BackendSelector::new()
            .backend_name("winit".into())
            .renderer_name(renderer.into())
            .with_winit_window_attributes_hook(window_frame::attributes)
            .select()
        {
            Ok(()) => {
                GPU_RENDERING.store(
                    renderer == "skia" || renderer == "femtovg",
                    std::sync::atomic::Ordering::Relaxed,
                );
                return;
            }
            Err(error) => eprintln!("harness-slint: {renderer} renderer unavailable: {error}"),
        }
    }
}

/// The window is drawn by a GPU renderer (set once, when the renderer is chosen).
pub(super) static GPU_RENDERING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Whether a GPU renderer draws the window. On the CPU renderer whole-window animation is costly, so an
/// animated wallpaper shows fewer frames there.
pub(crate) fn gpu_rendering() -> bool {
    GPU_RENDERING.load(std::sync::atomic::Ordering::Relaxed)
}

/// `general.gpuRendering` of the settings file. The renderer is chosen before the window and the settings
/// store exist, so this reads the file directly: `<base>/settings.json`, or the one in the relocated data
/// directory named by `<base>/relocated.json`. A missing or unreadable value means "on" (the default).
pub(super) fn gpu_rendering_setting(base: &Path) -> bool {
    let read = |path: &Path| -> Option<serde_json::Value> {
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
    };
    let data_dir = read(&base.join(relocation::POINTER_FILE))
        .and_then(|v| v.get("dataDir").and_then(|d| d.as_str()).map(PathBuf::from))
        .filter(|dir| dir.join(relocation::DATA_MARKER).exists())
        .unwrap_or_else(|| base.to_path_buf());
    read(&data_dir.join(harness_settings::SETTINGS_FILE))
        .and_then(|v| {
            v.pointer("/general/gpuRendering")
                .and_then(serde_json::Value::as_bool)
        })
        .unwrap_or(true)
}

/// Gates only (automation builds): `HARNESS_UI_WINDOW_SIZE=<w>x<h>` opens the window at that logical size, so the
/// narrow settings layout can be driven without a resize tool.
#[cfg(feature = "automation")]
pub(super) fn test_window_size(window: &ui::AppWindow) {
    let Some(value) = std::env::var("HARNESS_UI_WINDOW_SIZE").ok() else {
        return;
    };
    let Some((w, h)) = value.split_once('x') else {
        return;
    };
    if let (Ok(w), Ok(h)) = (w.trim().parse::<f32>(), h.trim().parse::<f32>()) {
        slint::ComponentHandle::window(window).set_size(slint::LogicalSize::new(w, h));
    }
}

/// Startup (spec §6.3): applies a relocation scheduled by the previous run, resolves the data directory and opens
/// the one `SettingsStore` of the process, before the first frame (so it already has the stored theme and language).
pub(super) fn boot(base: &Path) -> Option<(PathBuf, Arc<SettingsStore>, PathBuf)> {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("harness-slint: cannot start boot runtime: {error}");
            return None;
        }
    };
    runtime.block_on(async {
        if let Err(error) = relocation::apply_pending(base).await {
            eprintln!("harness-slint: data directory move failed, keeping the old one: {error}");
        }
        let data_dir = relocation::resolve_data_dir(base).await.data_dir;
        let store = match SettingsStore::open(&data_dir).await {
            Ok(store) => store,
            Err(error) => {
                eprintln!("harness-slint: settings store failed: {error}");
                return None;
            }
        };
        let last = store.snapshot().general.last_workspace.clone();
        let workspace = workspace(last.as_deref());
        let resolved = workspace.to_string_lossy().into_owned();
        if last.as_deref() != Some(resolved.as_str()) {
            if let Err(error) = store
                .patch(|s| s.general.last_workspace = Some(resolved))
                .await
            {
                eprintln!("harness-slint: cannot store the workspace: {error}");
            }
        }
        Some((data_dir, Arc::new(store), workspace))
    })
}

/// Helper modes of the single executable (phase-9-parity.md §9.9). Checked before any Slint, settings or tokio
/// setup: a helper process only speaks MCP / ACP on its piped stdio. Under the Windows GUI subsystem the parent's
/// pipes still arrive through `STARTF_USESTDHANDLES`, so std/tokio stdio work unchanged.
pub(super) fn helper_mode() -> Option<ExitCode> {
    use harness_agent_hub::helpers;
    let mut args = std::env::args().skip(1);
    let mode = args.next()?;
    match mode.as_str() {
        helpers::FLAG_MEMORY_MCP => Some(helpers::run_memory_mcp(args)),
        helpers::FLAG_DELEGATE => Some(helpers::run_delegate_mcp()),
        helpers::FLAG_TOOL_GATEWAY => Some(helpers::run_tool_gateway()),
        helpers::FLAG_CONNECTOR_MCP => Some(helpers::run_connector_mcp(args)),
        helpers::FLAG_API_AGENT => Some(harness_api_agent::run()),
        helpers::FLAG_COMPUTER_MCP => Some(computer_mcp::run()),
        helpers::FLAG_WORK_MCP => Some(harness_work::run_mcp()),
        crate::install::FLAG_INSTALL => Some(crate::install::run_cli()),
        _ => None,
    }
}

pub(super) fn install_crash_log(base: &std::path::Path) {
    let dir = base.join("logs");
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        use std::io::Write;
        let when = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_owned();
        let trace = std::backtrace::Backtrace::force_capture();
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("crash.log"))
        {
            let _ = writeln!(
                file,
                "=== panic at unix {when} on thread {thread}\n{info}\n{trace}\n"
            );
        }
        previous(info);
    }));
}
