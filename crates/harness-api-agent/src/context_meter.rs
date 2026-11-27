//! ACP `usage_update` for the client's context meter.
//!
//! The agent keeps its visible history under `compress::VIEW_CHARS` by folding old messages itself, so
//! the window the meter shows is that budget plus what every request carries besides the history
//! (system prompt, tools). `used` is the provider's own prompt count when it reports one.

use serde_json::{json, Value};

use crate::compress::VIEW_CHARS;

/// Characters per token, the estimate used across the agent.
const CHARS_PER_TOKEN: u64 = 4;

/// The update after a model call: `input`/`output` tokens the provider reported (0 = not reported),
/// `request_chars` the size of everything sent, `view_chars` the size of the visible history.
pub fn update(input: u64, output: u64, request_chars: usize, view_chars: usize) -> Value {
    let chars = |n: usize| u64::try_from(n).unwrap_or(u64::MAX) / CHARS_PER_TOKEN;
    let request = chars(request_chars);
    let used = if input > 0 { input } else { request } + output;
    // 系统提示和工具定义不计入历史预算，窗口 = 历史预算 + 这部分开销。
    let overhead = request.saturating_sub(chars(view_chars));
    json!({
        "sessionUpdate": "usage_update",
        "used": used,
        "size": chars(VIEW_CHARS) + overhead,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_is_the_history_budget_plus_the_fixed_part() {
        let u = update(30_000, 500, 120_000, 80_000);
        assert_eq!(u["used"], 30_500);
        assert_eq!(u["size"], 100_000 + 10_000);
        // No usage from the provider: the request size stands in.
        assert_eq!(update(0, 0, 120_000, 80_000)["used"], 30_000);
    }
}
