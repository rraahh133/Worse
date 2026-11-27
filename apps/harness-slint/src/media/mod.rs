//! In-app viewer for images and videos named in the transcript (`MediaUi`, `ui/chat/media_viewer.slint`).
//! Pure Slint, no webview: images are decoded with the `image` crate, videos with the user's ffmpeg
//! (`video.rs`). Opened from the preview strip under an answer, a tool's image, or Ctrl+click on a media link.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use slint::{ComponentHandle, Rgba8Pixel, SharedPixelBuffer};

use crate::ui::{AppWindow, MediaUi};

mod playback;
mod probe;
mod video;

pub use probe::{placeholder, video_thumb};

const IMAGE_EXT: [&str; 6] = ["png", "jpg", "jpeg", "bmp", "gif", "webp"];
const VIDEO_EXT: [&str; 6] = ["mp4", "mov", "webm", "mkv", "avi", "m4v"];
/// Largest side of an image shown in the viewer.
const MAX_SIDE: u32 = 4096;

pub fn is_image_ext(ext: &str) -> bool {
    IMAGE_EXT.contains(&ext)
}

pub fn is_video_ext(ext: &str) -> bool {
    VIDEO_EXT.contains(&ext)
}

pub fn is_media_ext(ext: &str) -> bool {
    is_image_ext(ext) || is_video_ext(ext)
}

fn ext(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default()
}

pub fn is_media(path: &str) -> bool {
    is_media_ext(&ext(path))
}

struct State {
    window: slint::Weak<AppWindow>,
    gallery: Vec<String>,
    index: usize,
    player: Option<video::Player>,
    /// Bumped on every shown file; late results of an older one are dropped.
    generation: u64,
    /// A frame is queued for the UI thread: newer frames are skipped meanwhile.
    in_flight: Arc<AtomicBool>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|cell| cell.try_borrow_mut().ok()?.as_mut().map(f))
}

fn ui_do(f: impl FnOnce(&MediaUi)) {
    if let Some(window) = with(|s| s.window.upgrade()).flatten() {
        f(&window.global::<MediaUi>());
    }
}

pub fn install(window: &AppWindow) {
    STATE.with(|cell| {
        *cell.borrow_mut() = Some(State {
            window: window.as_weak(),
            gallery: Vec::new(),
            index: 0,
            player: None,
            generation: 0,
            in_flight: Arc::new(AtomicBool::new(false)),
        });
    });
    let ui = window.global::<MediaUi>();
    ui.on_view(|path| view(&path));
    ui.on_close(close);
    ui.on_toggle_play(playback::toggle_play);
    ui.on_seek(|fraction| playback::seek(f64::from(fraction)));
    ui.on_step(step);
    ui.on_open_external(|| {
        if let Some(path) = current() {
            crate::chat::open_path(&path);
        }
    });
    ui.on_reveal(|| {
        use harness_platform::PathRevealer;
        if let Some(path) = current() {
            let _ = harness_platform::desktop::SystemPathRevealer::new().reveal(Path::new(&path));
        }
    });
}

fn current() -> Option<String> {
    with(|s| s.gallery.get(s.index).cloned()).flatten()
}

/// Opens the viewer on `path`, with the other media of its transcript row as previous / next.
pub fn view(path: &str) {
    let gallery: Vec<String> = crate::bridge::gallery_of(path)
        .into_iter()
        .filter(|p| is_media(p))
        .collect();
    let index = gallery.iter().position(|p| p == path);
    let (gallery, index) = match index {
        Some(index) => (gallery, index),
        None => (vec![path.to_owned()], 0),
    };
    with(|s| {
        s.gallery = gallery;
        s.index = index;
    });
    show();
}

fn close() {
    with(|s| {
        s.player = None;
        s.generation += 1;
    });
    ui_do(|ui| {
        ui.set_open(false);
        ui.set_frame(slint::Image::default());
        ui.set_has_frame(false);
    });
}

fn step(delta: i32) {
    let moved = with(|s| {
        let count = s.gallery.len();
        if count < 2 {
            return false;
        }
        let (index, count) = (
            i64::try_from(s.index).unwrap_or(0),
            i64::try_from(count).unwrap_or(1),
        );
        let next = (index + i64::from(delta)).rem_euclid(count);
        s.index = usize::try_from(next).unwrap_or(0);
        true
    });
    if moved == Some(true) {
        show();
    }
}

/// Shows the gallery entry at the current index.
fn show() {
    let Some((path, index, count, generation)) = with(|s| {
        s.player = None;
        s.generation += 1;
        let path = s.gallery.get(s.index).cloned()?;
        Some((path, s.index, s.gallery.len(), s.generation))
    })
    .flatten() else {
        return;
    };
    let video = is_video_ext(&ext(&path));
    let name = Path::new(&path)
        .file_name()
        .map_or_else(|| path.clone(), |n| n.to_string_lossy().into_owned());
    ui_do(|ui| {
        ui.set_open(true);
        ui.set_name(name.as_str().into());
        ui.set_path(path.as_str().into());
        ui.set_video(video);
        ui.set_has_frame(false);
        ui.set_frame(slint::Image::default());
        ui.set_loading(true);
        ui.set_error("".into());
        ui.set_playing(false);
        ui.set_progress(0.0);
        ui.set_time("".into());
        ui.set_index(i32::try_from(index + 1).unwrap_or(1));
        ui.set_count(i32::try_from(count).unwrap_or(1));
    });
    let file = PathBuf::from(&path);
    if video {
        std::thread::spawn(move || {
            let info = probe::info(&file);
            let _ = slint::invoke_from_event_loop(move || video_ready(generation, &file, info));
        });
    } else {
        std::thread::spawn(move || {
            let decoded = decode_image(&file);
            let _ = slint::invoke_from_event_loop(move || image_ready(generation, decoded));
        });
    }
}

fn decode_image(path: &Path) -> Result<SharedPixelBuffer<Rgba8Pixel>, String> {
    let image = image::ImageReader::open(path)
        .and_then(image::ImageReader::with_guessed_format)
        .map_err(|e| e.to_string())?
        .decode()
        .map_err(|e| e.to_string())?;
    let image = if image.width() > MAX_SIDE || image.height() > MAX_SIDE {
        image.thumbnail(MAX_SIDE, MAX_SIDE)
    } else {
        image
    };
    let rgba = image.to_rgba8();
    Ok(SharedPixelBuffer::clone_from_slice(
        rgba.as_raw(),
        rgba.width(),
        rgba.height(),
    ))
}

fn is_current(generation: u64) -> bool {
    with(|s| s.generation == generation).unwrap_or(false)
}

fn image_ready(generation: u64, decoded: Result<SharedPixelBuffer<Rgba8Pixel>, String>) {
    if !is_current(generation) {
        return;
    }
    ui_do(|ui| {
        ui.set_loading(false);
        match decoded {
            Ok(buffer) => {
                ui.set_frame(slint::Image::from_rgba8(buffer));
                ui.set_has_frame(true);
            }
            Err(error) => ui.set_error(format!("This image cannot be shown: {error}").into()),
        }
    });
}

fn video_ready(generation: u64, path: &Path, info: Option<probe::Info>) {
    if !is_current(generation) {
        return;
    }
    let Some(info) = info else {
        let message = if probe::tool("ffmpeg").is_none() || probe::tool("ffprobe").is_none() {
            "Playing videos inside Harness needs ffmpeg on PATH. Open it with the default app instead."
        } else {
            "This video cannot be read. Open it with the default app instead."
        };
        ui_do(|ui| {
            ui.set_loading(false);
            ui.set_error(message.into());
        });
        return;
    };
    with(|s| s.player = Some(video::Player::new(path, info)));
    playback::start(generation, false);
}
