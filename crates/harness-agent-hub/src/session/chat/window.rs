//! The context window Claude Code really compacts at.
//!
//! The Claude ACP adapter reports the model's native window (1M for the long-context models) as `size`,
//! but Claude Code compacts at the per-model `modelSettings.<model>.autoCompactWindow` of its
//! `settings.json`. The context meter and Harness's own auto-compact read `size`, so they showed 1M and
//! never fired before Claude Code did.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde_json::Value;

use crate::accounts::CLAUDE_CONFIG_ENV;

/// Parsed `settings.json` per path, with the modification time it was read at.
type Cache = HashMap<PathBuf, (Option<SystemTime>, Option<Value>)>;

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

/// The model the usage is for: the adapter tags it in `_meta["_claude/model"]`.
pub(super) fn usage_model(update: &Value) -> Option<&str> {
    update.get("_meta")?.get("_claude/model")?.as_str()
}

/// `claude-opus-5-5[1m]` and `Claude-Opus-5-5` name the same model.
fn normalize(model: &str) -> String {
    let base = model.split('[').next().unwrap_or(model);
    base.trim().to_ascii_lowercase()
}

/// The `autoCompactWindow` set for `model` in a parsed `settings.json`.
fn window_in(settings: &Value, model: &str) -> Option<u64> {
    let wanted = normalize(model);
    let table = settings.get("modelSettings")?.as_object()?;
    let window = |v: &Value| v.get("autoCompactWindow").and_then(Value::as_u64);
    let exact = table
        .iter()
        .find(|(k, _)| normalize(k) == wanted)
        .and_then(|(_, v)| window(v));
    // 修复：适配器可能报带日期后缀的完整 id（claude-opus-5-5-20261101），按前缀再找一次。
    exact.or_else(|| {
        table
            .iter()
            .filter(|(k, _)| wanted.starts_with(&normalize(k)))
            .max_by_key(|(k, _)| k.len())
            .and_then(|(_, v)| window(v))
    })
}

/// Claude Code's config dir for an agent: its spec's `CLAUDE_CONFIG_DIR`, the process's, else `~/.claude`.
pub(super) fn config_dir(spec_env: &[(String, String)]) -> Option<PathBuf> {
    if let Some((_, dir)) = spec_env.iter().find(|(k, _)| k == CLAUDE_CONFIG_ENV) {
        return Some(PathBuf::from(dir));
    }
    if let Some(dir) = std::env::var_os(CLAUDE_CONFIG_ENV) {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?;
    Some(Path::new(&home).join(".claude"))
}

/// Reads (or reuses) `<dir>/settings.json`; a changed file is read again.
async fn settings(dir: &Path) -> Option<Value> {
    let path = dir.join("settings.json");
    let modified = tokio::fs::metadata(&path)
        .await
        .ok()
        .and_then(|m| m.modified().ok());
    if let Ok(guard) = CACHE.lock() {
        if let Some((at, value)) = guard.as_ref().and_then(|c| c.get(&path)) {
            if *at == modified {
                return value.clone();
            }
        }
    }
    let value = match modified {
        Some(_) => tokio::fs::read_to_string(&path)
            .await
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok()),
        None => None,
    };
    if let Ok(mut guard) = CACHE.lock() {
        guard
            .get_or_insert_with(HashMap::new)
            .insert(path, (modified, value.clone()));
    }
    value
}

/// `size` clamped to the window Claude Code compacts `model` at, when one is set.
pub(super) async fn effective_size(dir: &Path, model: &str, size: u64) -> u64 {
    match settings(dir).await.and_then(|s| window_in(&s, model)) {
        Some(window) if window > 0 => size.min(window),
        _ => size,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn the_window_is_found_for_the_reported_model() {
        let settings = json!({ "modelSettings": {
            "claude-opus-5-5": { "autoCompactWindow": 200_000 },
            "claude-fable-5-2": { "autoCompactWindow": 300_000 },
            "claude-sonnet-5-5": { "effort": "high" },
        }});
        assert_eq!(window_in(&settings, "claude-fable-5-2[1m]"), Some(300_000));
        assert_eq!(window_in(&settings, "Claude-Opus-5-5"), Some(200_000));
        assert_eq!(
            window_in(&settings, "claude-opus-5-5-20261101"),
            Some(200_000)
        );
        assert_eq!(window_in(&settings, "claude-sonnet-5-5"), None);
        assert_eq!(window_in(&settings, "gpt-5"), None);
        assert_eq!(window_in(&json!({}), "claude-opus-5-5"), None);
    }

    #[test]
    fn the_model_comes_from_the_usage_meta() {
        let update =
            json!({ "used": 1, "size": 2, "_meta": { "_claude/model": "claude-opus-5-5" } });
        assert_eq!(usage_model(&update), Some("claude-opus-5-5"));
        assert_eq!(usage_model(&json!({ "used": 1 })), None);
    }

    #[tokio::test]
    async fn the_size_is_clamped_to_the_compact_window() {
        let dir = std::env::temp_dir().join(format!("harness-window-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("settings.json"),
            r#"{"modelSettings":{"claude-fable-5-2":{"autoCompactWindow":300000}}}"#,
        )
        .unwrap();
        assert_eq!(
            effective_size(&dir, "claude-fable-5-2", 1_000_000).await,
            300_000
        );
        assert_eq!(
            effective_size(&dir, "claude-fable-5-2", 200_000).await,
            200_000
        );
        assert_eq!(effective_size(&dir, "other", 1_000_000).await, 1_000_000);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
