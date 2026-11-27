//! Generation speed of a turn ("tok/s"), shown beside the working line and under the finished answer
//! (Settings › Appearance › "Show generation speed").
//!
//! Two figures:
//!
//! - While the agent writes: an estimate from the text that arrives, at four characters per token, over the
//!   time text was actually arriving. No agent reports token counts in the middle of a turn. Shown with "≈".
//! - After the turn: for agents that report the output tokens of a prompt (ACP `session/prompt` result
//!   `usage`), those tokens over the time the model was working. Like Hermes (output tokens over API call
//!   time), numerator and denominator cover the same work: the reported count includes thinking and tool
//!   call arguments that never stream as answer text, so the time is the whole turn minus the time a tool
//!   was running or waiting for an approval, not only the time answer text arrived. Agents that report
//!   nothing keep the estimate, still marked "≈".
//!
//! The live clock only runs while text arrives: a gap of more than two seconds (a tool is running, the agent
//! waits for an approval) is not generation time.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use slint::ComponentHandle;

use crate::core::{ItemId, ToolStatus};
use crate::ui::{AppWindow, ChatUi};

/// Characters per token of the estimate.
const CHARS_PER_TOKEN: f64 = 4.0;
/// A pause longer than this between two pieces of text is not generation time.
const GAP: Duration = Duration::from_secs(2);
/// The live label is refreshed at most this often.
const REFRESH: Duration = Duration::from_millis(400);
/// Shorter bursts give no meaningful rate.
const MIN_STREAM: Duration = Duration::from_millis(300);

#[derive(Default)]
struct State {
    /// Characters of answer and thinking text of the running turn.
    chars: usize,
    /// Time during which text was arriving.
    streaming: Duration,
    last: Option<Instant>,
    shown: Option<Instant>,
    /// Row of the answer being written; it gets the final figure.
    row: Option<ItemId>,
    /// When the running turn started.
    started: Option<Instant>,
    /// Tool rows of the running turn that are running or waiting for an approval.
    tools: HashSet<ItemId>,
    /// Since when at least one tool runs.
    tools_since: Option<Instant>,
    /// Time during which at least one tool ran.
    tool_time: Duration,
    /// A finished turn whose token count may still arrive: its row and the model's working time.
    awaiting: Option<(ItemId, Duration)>,
    /// Start (unix ms, [`crate::turn_clock`]) of the turn counted here.
    turn: Option<i64>,
}

impl State {
    fn close_tools(&mut self, now: Instant) {
        if let Some(since) = self.tools_since.take() {
            self.tool_time += now.duration_since(since);
        }
    }
}

// 修复：原来只有一份计数，打开别的会话就清零，切回正在运行的会话时 tok/s 从头估算（数字乱跳）。
// 现在按会话各存一份，切换只换“当前显示的会话”，回来后接着原来的计数。
#[derive(Default)]
struct Speeds {
    /// Session shown in the chat: the functions below count for it.
    shown: Option<String>,
    sessions: HashMap<String, State>,
}

thread_local! {
    static SPEEDS: RefCell<Speeds> = RefCell::default();
}

/// Runs `f` on the count of the shown session.
fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    SPEEDS.with(|cell| {
        let mut speeds = cell.borrow_mut();
        let key = speeds.shown.clone().unwrap_or_default();
        f(speeds.sessions.entry(key).or_default())
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// The instant of `at` (unix ms), for a turn that may have started before it was shown.
fn instant_of(at: Option<i64>) -> Instant {
    let now = Instant::now();
    at.and_then(|at| u64::try_from(now_ms().saturating_sub(at)).ok())
        .and_then(|ago| now.checked_sub(Duration::from_millis(ago)))
        .unwrap_or(now)
}

/// Tokens per second for `tokens` over `time`, or `None` when the time was too short to say.
fn rate(tokens: f64, time: Duration) -> Option<f64> {
    (time >= MIN_STREAM && tokens > 0.0).then(|| tokens / time.as_secs_f64())
}

fn round(rate: f64) -> String {
    if rate >= 10.0 {
        format!("{rate:.0}")
    } else {
        format!("{rate:.1}")
    }
}

/// Label of an estimate: "≈ 38 tok/s".
fn estimated(chars: usize, streaming: Duration) -> Option<String> {
    rate(chars as f64 / CHARS_PER_TOKEN, streaming).map(|r| format!("≈ {} tok/s", round(r)))
}

/// Label of a reported count over the model's working time: "41 tok/s · 85 output tokens".
fn exact(tokens: u64, working: Duration) -> Option<String> {
    rate(tokens as f64, working).map(|r| format!("{} tok/s · {tokens} output tokens", round(r)))
}

/// The model's working time of a turn: its wall time minus the time a tool ran.
fn working(wall: Duration, tool_time: Duration) -> Duration {
    wall.saturating_sub(tool_time)
}

/// The shown session runs the turn that started at `turn` (unix ms): a new turn starts the count over, the
/// same turn (shown again, another attempt) keeps it.
pub(crate) fn start(window: &AppWindow, turn: Option<i64>) {
    let fresh = with(|s| {
        if turn.is_some() && s.turn == turn && s.started.is_some() {
            return false;
        }
        let awaiting = s.awaiting.take();
        *s = State {
            started: Some(instant_of(turn)),
            awaiting,
            turn,
            ..State::default()
        };
        true
    });
    if fresh {
        window
            .global::<ChatUi>()
            .set_speed(slint::SharedString::new());
    }
}

/// Another session is shown (`None`: none): the count of the one left is kept for when it is shown again.
pub(crate) fn show(window: &AppWindow, session: Option<&str>) {
    SPEEDS.with(|cell| {
        let mut speeds = cell.borrow_mut();
        speeds.shown = session.map(str::to_owned);
        let now = Instant::now();
        speeds.sessions.retain(|_, s| {
            // 重新打开会话时条目 id 会重新分配：旧的回答行、工具行 id 作废，只保留回合的字数和时间。
            s.awaiting = None;
            s.row = None;
            s.close_tools(now);
            s.tools.clear();
            s.started.is_some()
        });
    });
    window
        .global::<ChatUi>()
        .set_speed(slint::SharedString::new());
}

/// A tool row of the running turn appeared or changed status.
pub(crate) fn tool(id: ItemId, status: ToolStatus) {
    with(|s| {
        if s.started.is_none() {
            return;
        }
        let now = Instant::now();
        if matches!(status, ToolStatus::Running | ToolStatus::AwaitingPermission) {
            if s.tools.insert(id) && s.tools.len() == 1 {
                s.tools_since = Some(now);
            }
        } else if s.tools.remove(&id) && s.tools.is_empty() {
            s.close_tools(now);
        }
    });
}

/// Text of the running turn arrived: answer text of `row`, or thinking text (`None`).
pub(crate) fn text(window: &AppWindow, row: Option<ItemId>, chars: usize) {
    let label = with(|s| {
        let now = Instant::now();
        if let Some(last) = s.last {
            let gap = now.duration_since(last);
            if gap <= GAP {
                s.streaming += gap;
            }
        }
        s.last = Some(now);
        s.chars += chars;
        if row.is_some() {
            s.row = row;
        }
        if s.shown.is_some_and(|t| now.duration_since(t) < REFRESH) {
            return None;
        }
        s.shown = Some(now);
        let sample = rate(s.chars as f64 / CHARS_PER_TOKEN, s.streaming);
        if let Some(sample) = sample {
            harness_perf::counter_float(harness_perf::Counter::TokensPerSecond, sample);
        }
        estimated(s.chars, s.streaming)
    });
    if let Some(label) = label {
        window.global::<ChatUi>().set_speed(label.as_str().into());
    }
}

/// Once a second while a turn runs: the live label goes away when no text arrived for a while (a tool is
/// running), so it never shows a stale rate.
pub(crate) fn tick(window: &AppWindow) {
    let stale = with(|s| s.last.map_or(true, |t| t.elapsed() > GAP));
    let ui = window.global::<ChatUi>();
    if stale && !ui.get_speed().is_empty() {
        ui.set_speed(slint::SharedString::new());
    }
}

/// The turn ended: the estimate for its answer row, which stays until a reported count arrives ([`reported`]).
pub(crate) fn finish(window: &AppWindow) -> Option<(ItemId, String)> {
    window
        .global::<ChatUi>()
        .set_speed(slint::SharedString::new());
    with(|s| {
        let now = Instant::now();
        s.close_tools(now);
        s.tools.clear();
        let wall = s.started.take().map(|t| now.duration_since(t));
        let row = s.row.take()?;
        let time = wall.map_or(s.streaming, |wall| working(wall, s.tool_time));
        s.awaiting = Some((row, time));
        estimated(s.chars, s.streaming).map(|label| (row, label))
    })
}

/// The agent reported the output tokens of the turn that just ended: the reported figure for its answer row.
pub(crate) fn reported(tokens: u64) -> Option<(ItemId, String)> {
    with(|s| {
        let (row, time) = s.awaiting.take()?;
        if let Some(sample) = rate(tokens as f64, time) {
            harness_perf::counter_float(harness_perf::Counter::TokensPerSecond, sample);
        }
        exact(tokens, time).map(|label| (row, label))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_uses_four_characters_per_token() {
        assert_eq!(
            estimated(800, Duration::from_secs(5)).as_deref(),
            Some("≈ 40 tok/s")
        );
        assert_eq!(
            estimated(60, Duration::from_secs(4)).as_deref(),
            Some("≈ 3.8 tok/s")
        );
    }

    #[test]
    fn reported_tokens_are_divided_by_the_model_working_time() {
        assert_eq!(
            exact(85, Duration::from_secs(2)).as_deref(),
            Some("42 tok/s · 85 output tokens")
        );
        let time = working(
            Duration::from_millis(324_900),
            Duration::from_millis(124_900),
        );
        assert_eq!(
            exact(9303, time).as_deref(),
            Some("47 tok/s · 9303 output tokens")
        );
        assert_eq!(
            working(Duration::from_secs(3), Duration::from_secs(5)),
            Duration::ZERO
        );
    }

    #[test]
    fn too_short_or_empty_gives_nothing() {
        assert_eq!(estimated(400, Duration::from_millis(100)), None);
        assert_eq!(estimated(0, Duration::from_secs(3)), None);
        assert_eq!(exact(0, Duration::from_secs(3)), None);
        assert_eq!(exact(50, Duration::ZERO), None);
    }
}
