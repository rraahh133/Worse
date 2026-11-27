//! Video playback state on the UI thread: start / pause / seek, frames from the reader thread, the clock.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::{is_current, ui_do, video, with};

/// Starts (or, with `still`, previews) the player of `generation`.
pub(super) fn start(generation: u64, still: bool) {
    let in_flight = with(|s| s.in_flight.clone());
    let Some(in_flight) = in_flight else { return };
    let report = move |report: video::Report| on_report(generation, &in_flight, report);
    let result = with(|s| s.player.as_mut().map(|p| p.start(still, report))).flatten();
    if let Some(Err(error)) = result {
        ui_do(|ui| {
            ui.set_loading(false);
            ui.set_error(error.into());
        });
        return;
    }
    let playing = !still;
    ui_do(|ui| ui.set_playing(playing));
    if playing {
        tick(generation);
    }
}

/// Reader thread → UI thread; at most one frame is in flight, older ones are dropped.
fn on_report(generation: u64, in_flight: &Arc<AtomicBool>, report: video::Report) {
    match report {
        video::Report::Frame(buffer, at) => {
            if in_flight.swap(true, Ordering::SeqCst) {
                return;
            }
            let flag = in_flight.clone();
            let _ = slint::invoke_from_event_loop(move || {
                flag.store(false, Ordering::SeqCst);
                if !is_current(generation) {
                    return;
                }
                ui_do(|ui| {
                    ui.set_frame(slint::Image::from_rgba8(buffer));
                    ui.set_has_frame(true);
                    ui.set_loading(false);
                });
                set_time(at);
            });
        }
        video::Report::End { played } => {
            let _ = slint::invoke_from_event_loop(move || {
                if !is_current(generation) {
                    return;
                }
                let still =
                    with(|s| s.player.as_ref().is_some_and(|p| !p.playing())).unwrap_or(false);
                if still {
                    return;
                }
                with(|s| {
                    if let Some(p) = s.player.as_mut() {
                        p.ended();
                    }
                });
                ui_do(|ui| {
                    ui.set_playing(false);
                    ui.set_loading(false);
                    if !played {
                        ui.set_error(
                            "This video cannot be played. Open it with the default app instead."
                                .into(),
                        );
                    }
                });
                let end = with(|s| s.player.as_ref().map(video::Player::position)).flatten();
                if let Some(end) = end {
                    set_time(end);
                }
            });
        }
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // 非负、有限的秒数
fn clock(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

#[allow(clippy::cast_possible_truncation)] // 进度在 0..1 之间
fn set_time(at: f64) {
    let Some(duration) = with(|s| s.player.as_ref().map(|p| p.info.duration)).flatten() else {
        return;
    };
    let progress = if duration > 0.0 {
        (at / duration).clamp(0.0, 1.0)
    } else {
        0.0
    };
    ui_do(|ui| {
        ui.set_progress(progress as f32);
        ui.set_time(format!("{} / {}", clock(at), clock(duration)).into());
    });
}

/// Keeps the clock moving while playing (frames may be dropped).
fn tick(generation: u64) {
    slint::Timer::single_shot(std::time::Duration::from_millis(250), move || {
        if !is_current(generation) {
            return;
        }
        let pos = with(|s| {
            s.player
                .as_ref()
                .filter(|p| p.playing())
                .map(video::Player::position)
        })
        .flatten();
        if let Some(pos) = pos {
            set_time(pos);
            tick(generation);
        }
    });
}

pub(super) fn toggle_play() {
    let Some(generation) = with(|s| s.generation) else {
        return;
    };
    let playing = with(|s| s.player.as_ref().map(video::Player::playing)).flatten();
    match playing {
        Some(true) => {
            with(|s| {
                if let Some(p) = s.player.as_mut() {
                    p.pause();
                }
            });
            ui_do(|ui| ui.set_playing(false));
        }
        Some(false) => start(generation, false),
        None => {}
    }
}

pub(super) fn seek(fraction: f64) {
    let Some(generation) = with(|s| s.generation) else {
        return;
    };
    let Some(was) = with(|s| s.player.as_mut().map(|p| p.seek_to(fraction))).flatten() else {
        return;
    };
    let pos = with(|s| s.player.as_ref().map(video::Player::position)).flatten();
    if let Some(pos) = pos {
        set_time(pos);
    }
    start(generation, !was);
}
