//! Harness native UI (Slint), Phase 8 round 1. Spec: `docs/specs/harness/phase-8-ui.md`.
//!
//! Main thread: Slint event loop. One `harness-core` thread: tokio current-thread runtime with the core,
//! the event pump and the file lister.

mod agy_cred;
mod agy_usage;
mod bridge;
mod chat;
mod computer_mcp;
mod core;
mod files;
mod gitui;
mod highlight;
mod import;
mod inbox;
mod install;
mod live;
mod live_update;
mod machine_view;
mod markdown;
mod media;
mod orchestra;
mod panels;

mod codex_usage;
mod perf;
mod plan_usage;
mod plan_view;
mod plugins;
mod release_update;
mod remote;
mod security;
mod settings;
mod setup;
mod skills;
mod speed;
mod turn_clock;
mod wallpaper;
mod window_frame;
mod workarea;

use harness_ui as ui;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use harness_settings::{relocation, SettingsStore};
use slint::ComponentHandle;
use tokio::sync::mpsc;

mod chrome;
#[allow(clippy::too_many_lines)]
mod startup;

use chrome::install_window_chrome;
pub(crate) use startup::gpu_rendering;
#[cfg(feature = "automation")]
use startup::test_window_size;
use startup::{
    boot, harness_home, helper_mode, install_crash_log, install_restart_for_update,
    prepare_process, relaunch,
};
#[allow(clippy::too_many_lines)]
#[must_use = "return the exit code from main"]
pub fn run() -> ExitCode {
    if let Some(code) = helper_mode() {
        return code;
    }
    // Live update: run from a shadow copy so the build output stays writable.
    if let Some(code) = live_update::shadow_launch() {
        return code;
    }
    // Then drop the shadow variables from the environment, so children do not inherit them.
    live_update::take_shadow_env();
    prepare_process();
    let window = match ui::AppWindow::new() {
        Ok(window) => window,
        Err(error) => {
            eprintln!("harness-slint: cannot create window: {error}");
            return ExitCode::FAILURE;
        }
    };
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (files_tx, files_rx) = mpsc::unbounded_channel();
    let (settings_tx, settings_rx) = mpsc::unbounded_channel();
    let base = harness_home();
    install_crash_log(&base);
    let use_fake = std::env::var("HARNESS_UI_CORE").is_ok_and(|v| v == "fake");
    let Some((data_dir, store, workspace_dir)) = boot(&base) else {
        return ExitCode::FAILURE;
    };
    let core_workspace = workspace_dir.clone();
    let initial = store.snapshot();
    // 窗口框架只读启动时的设置：winit 不能在运行中可靠地增删窗口装饰，所以 app.slint 的 no-frame
    // 绑定 native-frame，设置页提示“重启生效”。
    window.set_native_frame(initial.appearance.native_frame);
    let worker = settings::Worker {
        store: Some(Arc::clone(&store)),
        core: cmd_tx.clone(),
        base: base.clone(),
        data_dir: data_dir.clone(),
        workspace: workspace_dir.clone(),
    };
    let home = data_dir.clone();
    let core_thread = std::thread::Builder::new()
        .name("harness-core".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("harness-slint: cannot start core runtime: {error}");
                    return;
                }
            };
            // One SettingsStore per process (R6): the core and the settings worker share the same Arc.
            let core = async move {
                if use_fake {
                    core::fake::run(home, store, cmd_rx, event_tx).await;
                } else {
                    core::hub::run(home, core_workspace, store, cmd_rx, event_tx).await;
                }
            };
            runtime.block_on(async move {
                tokio::join!(
                    core,
                    bridge::pump(event_rx),
                    files::serve(files_rx),
                    worker.serve(settings_rx)
                );
            });
        });
    let core_thread = match core_thread {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("harness-slint: cannot spawn core thread: {error}");
            return ExitCode::FAILURE;
        }
    };
    #[cfg(feature = "automation")]
    test_window_size(&window);
    let first_run_done = initial.setup.first_run_done;
    settings::install(&window, settings_tx.clone(), initial, &data_dir);
    settings::recall::install(&window, &data_dir, &workspace_dir);
    settings::graph::install(&window, &data_dir, &workspace_dir);
    settings::decision::install(&window, &data_dir);
    plugins::install(&window, settings_tx.clone(), &data_dir);
    media::install(&window);
    skills::install(&window, settings_tx, &data_dir);
    // §9.10：首次启动检查与 Set up agents 对话框。
    setup::install(&window, first_run_done);
    chat::install(&window, files_tx.clone(), &workspace_dir, &data_dir);
    machine_view::install(&window, &data_dir);

    perf::install(&window, &data_dir);
    bridge::install(&window, cmd_tx, files_tx, &workspace_dir);
    panels::install(&window);
    gitui::install(&window);
    orchestra::install(&window);
    inbox::install(&window);
    import::install(&window);
    remote::install(&window, &data_dir);
    plan_usage::install(&window);
    security::install(&window, &base);
    install_window_chrome(&window);
    window_frame::install(&window);
    install_restart_for_update(&window, &workspace_dir);
    install::install_ui(&window, &workspace_dir);
    if live_update::is_shadow() {
        live_update::cleanup_old_copies();
        live_update::watch(window.as_weak());
    } else if install::is_installed() {
        // 从便携版迁移过来：删掉 ~/.harness/bin 里旧的影子副本。
        live_update::cleanup_old_copies();
        install::cleanup_replaced_exe();
    }
    release_update::watch(window.as_weak());
    let result = window.run();
    panels::trace("main: event loop returned");
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(8));
        panels::trace("main: shutdown watchdog fired");
        relaunch();
        std::process::exit(0);
    });
    remote::shutdown();
    settings::teardown();
    bridge::teardown();
    chat::teardown();
    panels::teardown();
    security::teardown();
    drop(window);
    panels::trace("main: joining core thread");
    if core_thread.join().is_err() {
        eprintln!("harness-slint: core thread panicked");
        return ExitCode::FAILURE;
    }
    relaunch();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("harness-slint: event loop error: {error}");
            ExitCode::FAILURE
        }
    }
}
