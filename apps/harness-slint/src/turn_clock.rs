//! When the running turn of each session started, for the working line's "Thinking… 12s" timer.
//!
//! 修复：计时器原来是工作行里的本地计数（每秒 +1），切到别的会话再回来时工作行重新创建，秒数从 0 开始；
//! 端口也只转发当前打开会话的事件。现在核心线程为每个会话记下回合开始的时间（`TurnStarted`，打开会话时
//! 用快照里正在运行回合的创建时间补上），界面每秒按当前会话读取已经过的秒数，切换会话不再归零。
//!
//! Written by the core thread (all sessions) and the UI thread (fallback when only `Running` arrives, as with
//! the scripted fake core); read by the UI thread.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

struct Clock {
    /// Session shown in the chat.
    shown: Option<String>,
    /// Per session: the running turn's id (empty when unknown) and its start, unix milliseconds.
    starts: BTreeMap<String, (String, i64)>,
}

static CLOCK: Mutex<Clock> = Mutex::new(Clock {
    shown: None,
    starts: BTreeMap::new(),
});

fn clock() -> MutexGuard<'static, Clock> {
    CLOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Turn `turn` of `session` runs since `at` (unix ms; now when `None`). Another attempt of the same turn
/// (quota failover) keeps the first start.
pub(crate) fn started(session: &str, turn: &str, at: Option<i64>) {
    let mut clock = clock();
    if clock
        .starts
        .get(session)
        .is_some_and(|(known, _)| known == turn)
    {
        return;
    }
    let at = at.unwrap_or_else(now_ms);
    clock
        .starts
        .insert(session.to_owned(), (turn.to_owned(), at));
}

/// `session` runs a turn: starts its clock now unless one already runs.
pub(crate) fn ensure(session: &str) {
    let now = now_ms();
    clock()
        .starts
        .entry(session.to_owned())
        .or_insert_with(|| (String::new(), now));
}

/// The turn of `session` finished or paused.
pub(crate) fn ended(session: &str) {
    clock().starts.remove(session);
}

/// Start (unix ms) of the running turn of `session`.
pub(crate) fn start_of(session: &str) -> Option<i64> {
    clock().starts.get(session).map(|(_, at)| *at)
}

/// The chat now shows `session` (`None`: no session).
pub(crate) fn show(session: Option<&str>) {
    clock().shown = session.map(str::to_owned);
}

/// Seconds the shown session's turn has been running, or -1 when unknown.
pub(crate) fn shown_secs() -> i32 {
    let clock = clock();
    let Some(at) = clock
        .shown
        .as_ref()
        .and_then(|session| clock.starts.get(session))
        .map(|(_, at)| *at)
    else {
        return -1;
    };
    secs_since(at, now_ms())
}

fn secs_since(at: i64, now: i64) -> i32 {
    i32::try_from(now.saturating_sub(at).max(0) / 1000).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_count_from_the_start() {
        assert_eq!(secs_since(10_000, 12_999), 2);
        assert_eq!(secs_since(10_000, 9_000), 0);
    }

    #[test]
    fn switching_sessions_keeps_each_start() {
        started("t-a", "turn-1", Some(1_000));
        started("t-b", "turn-9", Some(5_000));
        // 同一回合的下一次尝试（额度切换）保留第一次的开始时间。
        started("t-a", "turn-1", Some(3_000));
        assert_eq!(start_of("t-a"), Some(1_000));
        // 新回合重新计时。
        started("t-a", "turn-2", Some(4_000));
        assert_eq!(start_of("t-a"), Some(4_000));
        ensure("t-b");
        assert_eq!(start_of("t-b"), Some(5_000));
        ended("t-a");
        ended("t-b");
        assert_eq!(start_of("t-a"), None);
    }
}
