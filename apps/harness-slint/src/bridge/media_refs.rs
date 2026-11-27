//! Images and videos an agent answer names: Markdown link targets, `code` spans and bare paths that point to an
//! existing local media file. They show as a preview strip under the answer and open in the in-app viewer, so the
//! user does not have to find the file in a folder first.

use std::path::{Path, PathBuf};

use super::UiState;
use crate::core::ItemId;

/// At most this many files per answer.
const MAX_REFS: usize = 12;

/// Lower-case extension of `path`.
fn extension(path: &str) -> Option<String> {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
}

fn is_media(path: &str) -> bool {
    extension(path).is_some_and(|e| crate::media::is_media_ext(&e))
}

/// Raw candidates in reading order: link targets, code spans, then whitespace separated words.
fn candidates(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("](") {
        let after = &rest[at + 2..];
        let Some(end) = after.find(')') else { break };
        out.push(after[..end].trim().trim_matches(['<', '>']).to_owned());
        rest = &after[end..];
    }
    for (index, span) in text.split('`').enumerate() {
        if index % 2 == 1 && !span.contains('\n') {
            out.push(span.trim().to_owned());
        }
    }
    let separators =
        |c: char| c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']' | '`' | '"' | '<' | '>');
    for word in text.split(separators) {
        let word = word.trim_end_matches(['.', ',', ';', ':', '!', '?', '\'']);
        out.push(word.to_owned());
    }
    out
}

/// A candidate as a local path (`file://` URIs and URL-encoded names decoded, `:12` suffixes dropped).
fn local_path(raw: &str, workspace: &Path) -> Option<PathBuf> {
    let raw = raw.trim().trim_matches(['"', '\'']);
    if raw.is_empty() || (raw.contains("://") && !raw.starts_with("file:")) {
        return None;
    }
    let raw = crate::chat::strip_location_suffix(raw);
    if !is_media(raw) {
        return None;
    }
    let path = if let Some(rest) = raw.strip_prefix("file:") {
        let uri = if rest.starts_with("//") {
            format!("file:{rest}")
        } else {
            format!("file:///{rest}")
        };
        url::Url::parse(&uri).ok()?.to_file_path().ok()?
    } else if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        let joined = raw
            .split(['/', '\\'])
            .filter(|c| !c.is_empty() && *c != ".")
            .fold(workspace.to_path_buf(), |acc, c| acc.join(c));
        if joined.is_file() || !raw.contains('%') {
            joined
        } else {
            url::Url::from_directory_path(workspace)
                .ok()?
                .join(raw)
                .ok()?
                .to_file_path()
                .ok()?
        }
    };
    path.is_file().then_some(path)
}

/// Existing media files named in `text`, first mention first, without duplicates.
pub(super) fn find(text: &str, workspace: &Path) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for raw in candidates(text) {
        let Some(path) = local_path(&raw, workspace) else {
            continue;
        };
        let path = path.to_string_lossy().into_owned();
        let key = path.to_ascii_lowercase().replace('/', "\\");
        if !found
            .iter()
            .any(|p| p.to_ascii_lowercase().replace('/', "\\") == key)
        {
            found.push(path);
            if found.len() >= MAX_REFS {
                break;
            }
        }
    }
    found
}

impl UiState {
    /// After an answer finished: preview strip of the media files it names.
    pub(super) fn answer_media(&mut self, id: ItemId) {
        let Some(text) = self.texts.get(&id) else {
            return;
        };
        let paths = find(text, &self.workspace);
        if paths.is_empty() {
            return;
        }
        let (images, more) = self.row_images(id, &paths);
        self.update_row(id, |row| {
            row.images = images;
            row.images_more = more;
        });
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::find;

    #[test]
    fn finds_links_code_and_bare_paths_once() {
        let dir = std::env::temp_dir().join(format!("harness-media-refs-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("out")).unwrap();
        std::fs::write(dir.join("out").join("a b.mp4"), b"x").unwrap();
        std::fs::write(dir.join("shot.png"), b"x").unwrap();
        let abs = dir.join("shot.png").to_string_lossy().into_owned();
        let text = format!(
            "Saved [clip](out/a%20b.mp4) and `{abs}`, also {abs}. Missing: `gone.png`, [x](https://e.com/a.png)"
        );
        let found = find(&text, &dir);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found[0].ends_with("a b.mp4"));
        assert!(found[1].ends_with("shot.png"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
