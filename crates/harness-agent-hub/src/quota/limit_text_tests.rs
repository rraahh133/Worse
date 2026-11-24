#![allow(clippy::unwrap_used)]
use super::*;
use chrono::{Datelike, Local, TimeZone, Timelike};

#[test]
fn claude_five_hour_limit_is_quota_with_reset() {
    let info = AgentErrorInfo {
        code: Some(-32603),
        message: "Internal error: You've hit your 5-hour limit · resets 10:40am (Asia/Bangkok)"
            .into(),
        data: None,
    };
    match classify_generic(&info, ErrorDialect::ClaudeCode) {
        ErrorClass::QuotaExhausted { resets_at } => {
            let at = Local.timestamp_opt(resets_at.unwrap(), 0).unwrap();
            assert_eq!((at.hour(), at.minute()), (10, 40));
        }
        other => panic!("expected quota, got {other:?}"),
    }
}

#[test]
fn clock_reset_rolls_to_tomorrow_when_passed() {
    let now = Local.with_ymd_and_hms(2026, 11, 15, 12, 0, 0).unwrap();
    let at = parse_clock_from_rest("10:40am", now).unwrap();
    assert_eq!(
        at,
        Local
            .with_ymd_and_hms(2026, 11, 16, 10, 40, 0)
            .unwrap()
            .timestamp()
    );
    let pm = parse_clock_from_rest("3pm", now).unwrap();
    assert_eq!(
        pm,
        Local
            .with_ymd_and_hms(2026, 11, 15, 15, 0, 0)
            .unwrap()
            .timestamp()
    );
    // Recent time within 15 minutes resets immediately rather than jumping to tomorrow.
    let recent = Local.with_ymd_and_hms(2026, 11, 15, 12, 5, 0).unwrap();
    let at_recent = parse_clock_from_rest("12:01pm", recent).unwrap();
    assert_eq!(at_recent, recent.timestamp());
}

/// 固定"现在"为 10:00：真实时钟落在 13:31–13:46 时，"刚过去 15 分钟内"规则会返回当前时间，测试随运行时刻失败。
fn assert_clock_at_1331(message: &str) {
    let now = Local.with_ymd_and_hms(2026, 11, 24, 10, 0, 0).unwrap();
    let at = parse_reset_from_text_at(message, now.timestamp()).unwrap();
    let dt = Local.timestamp_opt(at, 0).unwrap();
    assert_eq!((dt.hour(), dt.minute()), (13, 31));
}

#[test]
fn codex_try_again_at_parsed() {
    let info = AgentErrorInfo {
        code: None,
        message: "You’ve hit your usage limit. Upgrade to Pro, or try again at 1:31 PM.".into(),
        data: None,
    };
    match classify_generic(&info, ErrorDialect::Codex) {
        ErrorClass::QuotaExhausted { resets_at: Some(_) } => assert_clock_at_1331(&info.message),
        other => panic!("expected quota with reset, got {other:?}"),
    }
}

#[test]
fn codex_try_again_at_with_date_and_url_parsed() {
    let info = AgentErrorInfo {
        code: None,
        message: "You’ve hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit https://chatgpt.com/settings/usage to purchase more credits or try again at 1:31 PM.\n\n".into(),
        data: None,
    };
    match classify_generic(&info, ErrorDialect::Codex) {
        ErrorClass::QuotaExhausted { resets_at: Some(_) } => assert_clock_at_1331(&info.message),
        other => panic!("expected quota with reset, got {other:?}"),
    }
}

#[test]
fn codex_try_again_at_with_date_parsed() {
    let now = Local.with_ymd_and_hms(2026, 11, 24, 10, 0, 0).unwrap();
    let at = parse_clock_from_rest("Nov 24th, 2026 1:31 PM.", now).unwrap();
    let dt = Local.timestamp_opt(at, 0).unwrap();
    assert_eq!(
        (dt.year(), dt.month(), dt.day(), dt.hour(), dt.minute()),
        (2026, 11, 24, 13, 31)
    );
}

#[test]
fn codex_try_again_in_relative_duration() {
    let text = "You've hit your usage limit. Try again in 20 minutes.";
    let parsed = parse_reset_from_text(text).unwrap();
    let now = now_secs();
    assert!(parsed >= now + 1200 - 5);
    assert!(parsed <= now + 1200 + 5);
}

#[test]
fn codex_24h_clock_parsed() {
    let now = Local.with_ymd_and_hms(2026, 11, 24, 10, 0, 0).unwrap();
    let at = parse_clock_from_rest("18:15", now).unwrap();
    let dt = Local.timestamp_opt(at, 0).unwrap();
    assert_eq!((dt.hour(), dt.minute()), (18, 15));
}

#[test]
fn codex_exhaustion_without_time_falls_back_to_5h() {
    let info = AgentErrorInfo {
        code: None,
        message: "You've hit your usage limit.".into(),
        data: None,
    };
    let before = now_secs();
    match classify_generic(&info, ErrorDialect::Codex) {
        ErrorClass::QuotaExhausted {
            resets_at: Some(at),
        } => {
            assert!(at >= before + 5 * 3600 - 5);
            assert!(at <= before + 5 * 3600 + 5);
        }
        other => panic!("expected quota with 5h fallback, got {other:?}"),
    }
}

#[test]
fn harness_error_quota_exhausted_fallback_only_for_codex() {
    let info = AgentErrorInfo {
        code: None,
        message: "Quota exceeded".into(),
        data: Some(serde_json::json!({
            "harnessErrorKind": "quota_exhausted"
        })),
    };
    let before = now_secs();
    match classify_generic(&info, ErrorDialect::Codex) {
        ErrorClass::QuotaExhausted {
            resets_at: Some(at),
        } => {
            assert!(at >= before + CODEX_USAGE_LIMIT_WINDOW_SECS - 5);
            assert!(at <= before + CODEX_USAGE_LIMIT_WINDOW_SECS + 5);
        }
        other => panic!("expected Codex quota with 5h fallback, got {other:?}"),
    }

    for non_codex in [
        ErrorDialect::ClaudeCode,
        ErrorDialect::Agy,
        ErrorDialect::Generic,
    ] {
        match classify_generic(&info, non_codex) {
            ErrorClass::QuotaExhausted { resets_at: None } => {}
            other => panic!("expected indefinite quota for {non_codex:?}, got {other:?}"),
        }
    }
}

#[test]
fn claude_exhaustion_without_time_is_indefinite() {
    let info = AgentErrorInfo {
        code: None,
        message: "You've hit your usage limit.".into(),
        data: None,
    };
    match classify_generic(&info, ErrorDialect::ClaudeCode) {
        ErrorClass::QuotaExhausted { resets_at: None } => {}
        other => panic!("expected indefinite quota for ClaudeCode, got {other:?}"),
    }
}

#[test]
fn quota_registry_set_preserves_indefinite_exhaustion() {
    let registry = QuotaRegistry::default();
    let agent = AgentId::new("claude-code");
    let indefinite = QuotaState::Exhausted {
        resets_at: None,
        reason: "monthly limit".into(),
    };
    registry.set(&agent, indefinite.clone());
    assert_eq!(registry.get(&agent), indefinite);
}

#[test]
fn find_resets_at_parses_string_numeric_relative_and_preserves_absolute() {
    let now = now_secs();

    for (data, secs) in [
        (serde_json::json!({ "retry_after": "120" }), 120),
        (serde_json::json!({ "reset_after_seconds": "3600" }), 3600),
        (serde_json::json!({ "retry_after": 60 }), 60),
    ] {
        assert!(find_resets_at(&data)
            .is_some_and(|at| { (now + secs..=now_secs() + secs).contains(&at) }));
    }

    // Negative retry_after is rejected (not relative, and not epoch)
    let data = serde_json::json!({ "retry_after": "-10" });
    assert_eq!(find_resets_at(&data), None);
    let data = serde_json::json!({ "retry_after": -10 });
    assert_eq!(find_resets_at(&data), None);

    // Genuine absolute ISO string in retry_after
    let data = serde_json::json!({ "retry_after": "2026-11-24T15:00:00Z" });
    assert_eq!(
        find_resets_at(&data),
        parse_iso8601_utc("2026-11-24T15:00:00Z")
    );

    // Genuine absolute epoch and ISO in resetsAt
    let data = serde_json::json!({ "resetsAt": 1_732_450_000 });
    assert_eq!(find_resets_at(&data), Some(1_732_450_000));
    let data = serde_json::json!({ "resetsAt": "1732450000" });
    assert_eq!(find_resets_at(&data), Some(1_732_450_000));
    let data = serde_json::json!({ "resetsAt": "2026-11-24T15:00:00Z" });
    assert_eq!(
        find_resets_at(&data),
        parse_iso8601_utc("2026-11-24T15:00:00Z")
    );
}
