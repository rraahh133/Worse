//! Logos for connector and plugin rows: downloaded once in the background (site icon, or the GitHub owner's
//! avatar), shrunk to 64 px and cached as PNG under `<HARNESS_HOME>/cache/icons/`. Rows without a logo show a
//! letter tile.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

/// A failed lookup is retried after this long.
const RETRY_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const SIZE: u32 = 64;

#[derive(Default)]
struct Icons {
    loaded: HashMap<String, Option<Image>>,
    pending: Vec<String>,
    running: bool,
}

thread_local! {
    static ICONS: RefCell<Icons> = RefCell::new(Icons::default());
}

/// Where the logo of a site or repository comes from: (cache key, candidate URLs).
fn source(homepage: &str) -> Option<(String, Vec<String>)> {
    let url = reqwest::Url::parse(homepage).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    if host == "github.com" {
        let owner = url.path_segments()?.find(|s| !s.is_empty())?.to_owned();
        let safe: String = owner
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        if safe.is_empty() {
            return None;
        }
        return Some((
            format!("gh-{}", safe.to_ascii_lowercase()),
            vec![format!("https://github.com/{safe}.png?size={SIZE}")],
        ));
    }
    let safe: String = host
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        .collect();
    let root = host.trim_start_matches("www.");
    let mut urls = vec![
        format!("https://{host}/apple-touch-icon.png"),
        format!("https://{host}/favicon.ico"),
        format!("https://{host}/favicon.png"),
    ];
    // 子域名（drive.google.com）常没有图标文件：再试主域名。
    let parts: Vec<&str> = root.split('.').collect();
    if parts.len() > 2 {
        let main = parts[parts.len() - 2..].join(".");
        urls.push(format!("https://www.{main}/apple-touch-icon.png"));
        urls.push(format!("https://www.{main}/favicon.ico"));
    }
    Some((safe, urls))
}

/// The logo of `homepage` when it is loaded; otherwise starts loading it and returns `None` (rows are pushed
/// again once it arrives).
pub(super) fn get(data_dir: &Path, homepage: &str) -> Option<Image> {
    let (key, urls) = source(homepage)?;
    let start = ICONS.with(|cell| {
        let mut icons = cell.borrow_mut();
        if let Some(image) = icons.loaded.get(&key) {
            return Err(image.clone());
        }
        if !icons.pending.contains(&key) {
            icons.pending.push(key.clone());
            icons.loaded.insert(key.clone(), None);
            let first = !icons.running;
            icons.running = true;
            return Ok(Some((first, key, urls)));
        }
        Ok(None)
    });
    match start {
        Err(image) => image,
        Ok(Some((first, key, urls))) => {
            queue(data_dir.to_path_buf(), key, urls, first);
            None
        }
        Ok(None) => None,
    }
}

type Job = (String, Vec<String>);

thread_local! {
    static QUEUE: RefCell<Vec<Job>> = const { RefCell::new(Vec::new()) };
}

fn queue(dir: PathBuf, key: String, urls: Vec<String>, start: bool) {
    QUEUE.with(|q| q.borrow_mut().push((key, urls)));
    if start {
        next(dir);
    }
}

/// Runs the queued jobs one batch at a time on a worker thread.
fn next(dir: PathBuf) {
    let batch: Vec<Job> = QUEUE.with(|q| std::mem::take(&mut *q.borrow_mut()));
    if batch.is_empty() {
        ICONS.with(|cell| cell.borrow_mut().running = false);
        return;
    }
    let cache = dir.join("cache").join("icons");
    super::load::run(
        move || {
            let client = client();
            batch
                .into_iter()
                .map(|(key, urls)| {
                    let pixels = cached(&cache, &key).or_else(|| {
                        client
                            .as_ref()
                            .and_then(|c| download(c, &cache, &key, &urls))
                    });
                    (key, pixels)
                })
                .collect::<Vec<_>>()
        },
        move |done| {
            ICONS.with(|cell| {
                let mut icons = cell.borrow_mut();
                for (key, pixels) in done {
                    icons.pending.retain(|k| *k != key);
                    icons.loaded.insert(key, pixels.map(Image::from_rgba8));
                }
            });
            super::rows::push();
            next(dir);
        },
    );
}

fn client() -> Option<reqwest::Client> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .user_agent("Mozilla/5.0 (Harness)")
        .build()
        .ok()
}

fn decode(bytes: &[u8]) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let image = image::load_from_memory(bytes).ok()?;
    if image.width() < 16 {
        return None;
    }
    let small = image
        .resize(SIZE, SIZE, image::imageops::FilterType::Triangle)
        .to_rgba8();
    Some(SharedPixelBuffer::clone_from_slice(
        small.as_raw(),
        small.width(),
        small.height(),
    ))
}

fn cached(cache: &Path, key: &str) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    std::fs::read(cache.join(format!("{key}.png")))
        .ok()
        .and_then(|b| decode(&b))
}

fn download(
    client: &reqwest::Client,
    cache: &Path,
    key: &str,
    urls: &[String],
) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let missing = cache.join(format!("{key}.none"));
    let recent = std::fs::metadata(&missing)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < RETRY_AFTER);
    if recent {
        return None;
    }
    let _ = std::fs::create_dir_all(cache);
    for url in urls {
        let bytes = super::load::block_on(async {
            let response = client.get(url).send().await.ok()?;
            if !response.status().is_success() {
                return None;
            }
            response.bytes().await.ok()
        })
        .flatten();
        let Some(pixels) = bytes.as_deref().and_then(decode) else {
            continue;
        };
        if let Some(png) =
            image::RgbaImage::from_raw(pixels.width(), pixels.height(), pixels.as_bytes().to_vec())
        {
            let _ = png.save_with_format(cache.join(format!("{key}.png")), image::ImageFormat::Png);
        }
        return Some(pixels);
    }
    let _ = std::fs::write(missing, b"");
    None
}
