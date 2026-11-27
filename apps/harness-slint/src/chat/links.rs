//! Ctrl+click on a Markdown link in the transcript: local paths and `file://` URIs open with the OS
//! default app. External URLs, UNC shares and remote `file://` hosts are ignored.

use super::hooks::{open_path, workspace_path};
use super::{with_state, Path, PathBuf};

/// Extensions the OS would run instead of open. A link in agent output must not start a program with
/// one click, so for these the containing folder opens instead.
const RUNNABLE: &[&str] = &[
    "exe", "com", "bat", "cmd", "ps1", "psm1", "vbs", "vbe", "js", "jse", "wsf", "wsh", "msi",
    "msp", "scr", "pif", "lnk", "hta", "cpl", "reg", "jar", "app", "sh", "command",
];

/// Strips `:12`, `:12-20` and `:L12-L20` location suffixes; a lone drive letter (`C:`) stays.
pub(crate) fn strip_location_suffix(mut path: &str) -> &str {
    let is_line = |value: &str| {
        let value = value.strip_prefix('L').unwrap_or(value);
        !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
    };
    while let Some((head, suffix)) = path.rsplit_once(':') {
        if head.len() == 1 && head.as_bytes()[0].is_ascii_alphabetic() {
            break;
        }
        let is_location = match suffix.split_once('-') {
            Some((first, last)) => is_line(first) && is_line(last),
            None => is_line(suffix),
        };
        if !is_location {
            break;
        }
        path = head;
    }
    path
}

/// Opens `target` unless it is runnable; a runnable file reveals its folder instead.
fn open_safe(target: &str) {
    let plain = strip_location_suffix(target);
    if crate::media::is_media(plain) {
        // Images / videos open in the in-app viewer instead of an outside program.
        let full = if Path::new(plain).is_absolute() {
            Some(PathBuf::from(plain))
        } else {
            with_state(|s| s.workspace.clone()).map(|w| workspace_path(&w, plain))
        };
        if let Some(full) = full.filter(|p| p.is_file()) {
            return crate::media::view(&full.to_string_lossy());
        }
    }
    let runnable = Path::new(plain)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| RUNNABLE.iter().any(|r| ext.eq_ignore_ascii_case(r)));
    if runnable {
        let parent = Path::new(plain)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        open_path(&parent);
    } else {
        open_path(target);
    }
}

/// `file://` URI → local path (`None` = not a local file URI).
fn file_uri(url: &url::Url) -> Option<Result<PathBuf, String>> {
    if url.scheme() != "file" {
        return None;
    }
    let host = url.host_str().filter(|h| !h.is_empty());
    let drive = host.filter(|h| {
        cfg!(windows) && h.len() == 2 && h.as_bytes()[0].is_ascii_alphabetic() && h.ends_with(':')
    });
    if let Some(drive) = drive {
        // `file://C:/x` puts the drive in the host.
        let fixed = format!("file:///{drive}{}", url.path());
        return Some(
            url::Url::parse(&fixed)
                .ok()
                .and_then(|u| u.to_file_path().ok())
                .ok_or_else(String::new),
        );
    }
    if host.is_some_and(|h| !h.eq_ignore_ascii_case("localhost")) {
        return Some(Err(String::new()));
    }
    // `file:relative/path` is no absolute URI: resolve it against the workspace.
    Some(
        url.to_file_path()
            .map_err(|()| url.path().trim_start_matches('/').to_owned()),
    )
}

/// Opens a Markdown link when it points to a local path or `file://` URI.
pub fn open_link(raw: &str) {
    let mut target = raw.trim().to_owned();
    if target.is_empty() || target.starts_with('#') {
        return;
    }
    if let Ok(url) = url::Url::parse(&target) {
        match file_uri(&url) {
            Some(Ok(path)) => return open_safe(&path.to_string_lossy()),
            Some(Err(relative)) if relative.is_empty() => return,
            Some(Err(relative)) => target = relative,
            // External URLs are not local files.
            None if target.contains("://") => return,
            None => {}
        }
    }
    if cfg!(windows) {
        if target.starts_with(r"\\") || target.starts_with("//") {
            return;
        }
        // "/C:/x" → "C:/x"; "/src/a.rs" is workspace-relative.
        if target.starts_with('/') {
            target.remove(0);
        }
    }
    let workspace = with_state(|s| s.workspace.clone());
    let exists = |t: &str| {
        (Path::new(t).is_absolute() && Path::new(t).exists())
            || workspace
                .as_deref()
                .is_some_and(|w| workspace_path(w, t).exists())
    };
    if exists(&target) {
        return open_safe(&target);
    }
    let target = strip_location_suffix(&target);
    let bytes = target.as_bytes();
    if cfg!(windows)
        && bytes.len() > 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && !matches!(bytes[2], b'/' | b'\\')
    {
        // Drive-relative "C:x": only when it resolves.
        if let Ok(path) = Path::new(target).canonicalize() {
            open_safe(&path.to_string_lossy());
        }
        return;
    }
    if Path::new(target).is_absolute() {
        let decoded = if Path::new(target).exists() || !target.contains(['%', '#', '?']) {
            PathBuf::from(target)
        } else {
            absolute_file_url_path(target).unwrap_or_else(|| PathBuf::from(target))
        };
        return open_safe(&decoded.to_string_lossy());
    }
    let Some(workspace) = workspace else {
        return;
    };
    if workspace_path(&workspace, target).exists() {
        return open_safe(target);
    }
    // URL-encoded relative names ("my%20file.md").
    let decoded = url::Url::from_directory_path(&workspace)
        .ok()
        .and_then(|base| base.join(&format!("./{target}")).ok())
        .filter(|url| url.scheme() == "file")
        .and_then(|url| url.to_file_path().ok());
    if let Some(path) = decoded {
        open_safe(&path.to_string_lossy());
    }
}

fn absolute_file_url_path(path: &str) -> Option<PathBuf> {
    let path = path.replace('\\', "/");
    let url = if cfg!(windows) && path.as_bytes().get(1) == Some(&b':') {
        format!("file:///{path}")
    } else {
        format!("file://{path}")
    };
    url::Url::parse(&url).ok()?.to_file_path().ok()
}

#[cfg(test)]
mod tests {
    use super::strip_location_suffix;

    #[test]
    fn location_suffixes_are_stripped() {
        assert_eq!(strip_location_suffix("src/a.rs:12"), "src/a.rs");
        assert_eq!(strip_location_suffix("src/a.rs:L3-L9"), "src/a.rs");
        assert_eq!(strip_location_suffix("src/a.rs:4:2"), "src/a.rs");
        assert_eq!(strip_location_suffix("C:"), "C:");
        assert_eq!(strip_location_suffix("C:/x/a.rs:7"), "C:/x/a.rs");
        assert_eq!(strip_location_suffix("note:todo"), "note:todo");
    }
}
