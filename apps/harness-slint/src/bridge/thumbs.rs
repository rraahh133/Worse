//! Small previews of images in the transcript: images attached to a user message, image files a tool read, and the
//! images / videos an answer names (a video shows its first frame, decoded with ffmpeg when it is installed).
//! Decoding runs on one worker thread; the UI thread keeps the decoded thumbnails and patches the rows.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{channel, Sender};
use std::sync::OnceLock;

use slint::{ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

use super::{with_state, UiState};
use crate::core::ItemId;
use crate::ui::ChatImage;

const MAX_SHOWN: usize = 4;
const MAX_FILE_BYTES: u64 = 40 * 1024 * 1024;
const MAX_CACHED: usize = 160;
const THUMB_WIDTH: u32 = 336;
const THUMB_HEIGHT: u32 = 216;

#[derive(Default)]
pub(super) struct Thumbs {
    /// Decoded thumbnails by path; `None` while decoding or when the file cannot be decoded.
    cache: HashMap<String, Option<slint::Image>>,
    rows: HashMap<ItemId, Vec<String>>,
}

impl Thumbs {
    pub(super) fn clear_rows(&mut self) {
        self.rows.clear();
        if self.cache.len() > MAX_CACHED {
            self.cache.clear();
        }
    }
}

fn ext(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default()
}

fn is_image(path: &str) -> bool {
    crate::media::is_image_ext(&ext(path))
}

fn is_media(path: &str) -> bool {
    crate::media::is_media_ext(&ext(path))
}

fn is_video(path: &str) -> bool {
    crate::media::is_video_ext(&ext(path))
}

fn resolve(raw: &str, workspace: &Path) -> PathBuf {
    let trimmed = raw.trim().trim_matches(['`', '"', '\'']);
    if Path::new(trimmed).is_absolute() {
        PathBuf::from(trimmed)
    } else {
        trimmed
            .split(['/', '\\'])
            .filter(|c| !c.is_empty() && *c != ".")
            .fold(workspace.to_path_buf(), |acc, c| acc.join(c))
    }
}

/// Image file named by a tool title such as `Read C:\shots\a.png`.
pub(super) fn tool_image(title: &str, workspace: &Path) -> Option<String> {
    let line = title.lines().next()?.trim();
    let after_verb = line.split_once(char::is_whitespace).map(|(_, rest)| rest);
    [after_verb, Some(line)]
        .into_iter()
        .flatten()
        .filter(|candidate| is_image(candidate.trim().trim_matches(['`', '"', '\''])))
        .map(|candidate| resolve(candidate, workspace))
        .find(|path| path.is_file())
        .map(|path| path.to_string_lossy().into_owned())
}

fn decode(path: &Path) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    if is_video(&path.to_string_lossy()) {
        // 没有 ffmpeg 或解码失败时用一块深色占位图，条带里仍显示播放按钮。
        return Some(
            crate::media::video_thumb(path, THUMB_WIDTH, THUMB_HEIGHT)
                .unwrap_or_else(crate::media::placeholder),
        );
    }
    if std::fs::metadata(path).ok()?.len() > MAX_FILE_BYTES {
        return None;
    }
    let image = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    let thumb = image.thumbnail(THUMB_WIDTH, THUMB_HEIGHT).to_rgba8();
    Some(SharedPixelBuffer::clone_from_slice(
        thumb.as_raw(),
        thumb.width(),
        thumb.height(),
    ))
}

fn worker() -> &'static Sender<String> {
    static WORKER: OnceLock<Sender<String>> = OnceLock::new();
    WORKER.get_or_init(|| {
        let (tx, rx) = channel::<String>();
        std::thread::spawn(move || {
            while let Ok(path) = rx.recv() {
                let buffer = decode(Path::new(&path));
                let _ = slint::invoke_from_event_loop(move || {
                    with_state(|s| s.thumb_ready(&path, buffer));
                });
            }
        });
        tx
    })
}

impl UiState {
    /// Previews of `paths` for row `id`: the decoded ones now, the others once the worker has them.
    pub(super) fn row_images(&mut self, id: ItemId, paths: &[String]) -> (ModelRc<ChatImage>, i32) {
        let paths: Vec<String> = paths
            .iter()
            .filter(|p| is_media(p))
            .map(|p| resolve(p, &self.workspace).to_string_lossy().into_owned())
            .collect();
        if paths.is_empty() {
            self.thumbs.rows.remove(&id);
            return (ModelRc::default(), 0);
        }
        for path in paths.iter().take(MAX_SHOWN) {
            if !self.thumbs.cache.contains_key(path) {
                self.thumbs.cache.insert(path.clone(), None);
                let _ = worker().send(path.clone());
            }
        }
        let shown = self.shown(&paths);
        self.thumbs.rows.insert(id, paths);
        shown
    }

    fn shown(&self, paths: &[String]) -> (ModelRc<ChatImage>, i32) {
        let images: Vec<ChatImage> = paths
            .iter()
            .take(MAX_SHOWN)
            .filter_map(|path| {
                let image = self.thumbs.cache.get(path)?.clone()?;
                Some(ChatImage {
                    path: path.as_str().into(),
                    image,
                    video: is_video(path),
                })
            })
            .collect();
        let more = i32::try_from(paths.len().saturating_sub(MAX_SHOWN)).unwrap_or(0);
        (ModelRc::from(Rc::new(VecModel::from(images))), more)
    }

    fn thumb_ready(&mut self, path: &str, buffer: Option<SharedPixelBuffer<Rgba8Pixel>>) {
        let Some(buffer) = buffer else {
            return;
        };
        self.thumbs
            .cache
            .insert(path.to_string(), Some(slint::Image::from_rgba8(buffer)));
        let rows: Vec<ItemId> = self
            .thumbs
            .rows
            .iter()
            .filter(|(_, paths)| paths.iter().any(|p| p == path))
            .map(|(id, _)| *id)
            .collect();
        for id in rows {
            let Some(paths) = self.thumbs.rows.get(&id) else {
                continue;
            };
            let (images, more) = self.shown(paths);
            self.update_row(id, |row| {
                row.images = images;
                row.images_more = more;
            });
        }
    }
}

/// Media files of the transcript row that shows `path` (the viewer's previous / next), else just `path`.
pub(crate) fn gallery_of(path: &str) -> Vec<String> {
    let mut found = None;
    with_state(|s| {
        found = s
            .thumbs
            .rows
            .values()
            .find(|paths| paths.iter().any(|p| p == path))
            .cloned();
    });
    found.unwrap_or_else(|| vec![path.to_owned()])
}
