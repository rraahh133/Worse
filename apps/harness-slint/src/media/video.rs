//! Video playback without a webview: `ffmpeg` decodes RGBA frames into a pipe, a reader thread paces them by the
//! wall clock and hands the newest one to the UI thread; `ffplay -nodisp` plays the sound track. Pause and seek
//! stop both processes and start them again at the new position, which keeps sound and picture in step.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use slint::{Rgba8Pixel, SharedPixelBuffer};

use super::probe::{self, Info};

/// Largest decoded frame; the window scales it further.
const MAX_W: u32 = 1280;
const MAX_H: u32 = 720;

/// What the reader thread reports to the UI thread.
pub(super) enum Report {
    /// A frame and the position (seconds) it shows.
    Frame(SharedPixelBuffer<Rgba8Pixel>, f64),
    /// The stream ended (end of file or ffmpeg failed); `played` = at least one frame came.
    End { played: bool },
}

/// Output size: `info` fitted into `MAX_W` x `MAX_H`, even numbers (yuv scalers want them).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // 缩放后的尺寸为正且不超过 MAX_W/MAX_H
pub(crate) fn out_size(info: &Info) -> (u32, u32) {
    let scale = (f64::from(MAX_W) / f64::from(info.width))
        .min(f64::from(MAX_H) / f64::from(info.height))
        .min(1.0);
    let even = |v: f64| ((v.round() as u32) / 2 * 2).max(2);
    (
        even(f64::from(info.width) * scale),
        even(f64::from(info.height) * scale),
    )
}

struct Run {
    stop: Arc<AtomicBool>,
    video: Child,
    audio: Option<Child>,
}

impl Drop for Run {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.video.kill();
        let _ = self.video.wait();
        if let Some(audio) = self.audio.as_mut() {
            let _ = audio.kill();
            let _ = audio.wait();
        }
    }
}

pub(super) struct Player {
    path: PathBuf,
    pub info: Info,
    /// Position (seconds) the current run started at, or the paused position.
    pos: f64,
    started: Option<Instant>,
    run: Option<Run>,
}

impl Player {
    pub fn new(path: &Path, info: Info) -> Self {
        Self {
            path: path.to_path_buf(),
            info,
            pos: 0.0,
            started: None,
            run: None,
        }
    }

    pub fn playing(&self) -> bool {
        self.run.is_some() && self.started.is_some()
    }

    /// Current position in seconds.
    pub fn position(&self) -> f64 {
        let now = self
            .started
            .map_or(self.pos, |t| self.pos + t.elapsed().as_secs_f64());
        if self.info.duration > 0.0 {
            now.min(self.info.duration)
        } else {
            now
        }
    }

    /// Starts decoding at the current position. `still` shows one frame and keeps the player paused.
    /// `report` runs on the reader thread for every frame and at the end.
    pub fn start(
        &mut self,
        still: bool,
        report: impl Fn(Report) + Send + 'static,
    ) -> Result<(), String> {
        self.run = None;
        let ffmpeg = probe::tool("ffmpeg").ok_or_else(|| "ffmpeg is not installed".to_owned())?;
        if self.info.duration > 0.0 && self.pos >= self.info.duration - 0.05 {
            self.pos = 0.0;
        }
        let (width, height) = out_size(&self.info);
        let mut command = probe::command(ffmpeg);
        command
            .args(["-v", "error", "-ss", &format!("{:.3}", self.pos), "-i"])
            .arg(&self.path)
            .args(["-an", "-sn", "-vf", &format!("scale={width}:{height}")])
            .args(["-f", "rawvideo", "-pix_fmt", "rgba"]);
        if still {
            command.args(["-frames:v", "1"]);
        }
        let mut video = command
            .arg("pipe:1")
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("ffmpeg could not start: {e}"))?;
        let stdout = video
            .stdout
            .take()
            .ok_or_else(|| "ffmpeg has no output".to_owned())?;
        let audio = if still {
            None
        } else {
            probe::tool("ffplay").and_then(|ffplay| {
                let at = format!("{:.3}", self.pos);
                probe::command(ffplay)
                    .args([
                        "-nodisp",
                        "-autoexit",
                        "-vn",
                        "-loglevel",
                        "quiet",
                        "-ss",
                        &at,
                    ])
                    .arg(&self.path)
                    .stdout(Stdio::null())
                    .spawn()
                    .ok()
            })
        };
        let stop = Arc::new(AtomicBool::new(false));
        let origin = Instant::now();
        let (start_pos, fps) = (self.pos, self.info.fps);
        let frame_len = (width as usize) * (height as usize) * 4;
        let thread_stop = stop.clone();
        std::thread::spawn(move || {
            let pace = Pace {
                fps,
                start_pos,
                origin,
                still,
            };
            read_frames(
                stdout,
                frame_len,
                (width, height),
                &pace,
                &thread_stop,
                &report,
            );
        });
        self.started = (!still).then_some(origin);
        self.run = Some(Run { stop, video, audio });
        Ok(())
    }

    /// Stops at the current position.
    pub fn pause(&mut self) {
        self.pos = self.position();
        self.started = None;
        self.run = None;
    }

    /// Jumps to `fraction` (0..1) of the duration; returns whether it was playing.
    pub fn seek_to(&mut self, fraction: f64) -> bool {
        let was = self.playing();
        self.pause();
        self.pos = (fraction.clamp(0.0, 1.0) * self.info.duration).max(0.0);
        was
    }

    /// The stream ran out: rest at the end.
    pub fn ended(&mut self) {
        self.pos = if self.info.duration > 0.0 {
            self.info.duration
        } else {
            self.position()
        };
        self.started = None;
        self.run = None;
    }
}

/// Timing of one run.
struct Pace {
    fps: f64,
    start_pos: f64,
    origin: Instant,
    still: bool,
}

fn read_frames(
    mut stdout: impl Read,
    frame_len: usize,
    (width, height): (u32, u32),
    pace: &Pace,
    stop: &AtomicBool,
    report: &dyn Fn(Report),
) {
    let mut buf = vec![0u8; frame_len];
    let mut index: u32 = 0;
    let mut played = false;
    while !stop.load(Ordering::SeqCst) {
        if stdout.read_exact(&mut buf).is_err() {
            break;
        }
        let at = f64::from(index) / pace.fps;
        if !pace.still {
            let due = pace.origin + Duration::from_secs_f64(at);
            let now = Instant::now();
            if due > now {
                std::thread::sleep(due - now);
            } else if now - due > Duration::from_millis(120) {
                // 跟不上时丢帧，保持和声音同步。
                index += 1;
                continue;
            }
        }
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let frame = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&buf, width, height);
        report(Report::Frame(frame, pace.start_pos + at));
        played = true;
        index += 1;
        if pace.still {
            return;
        }
    }
    if !stop.load(Ordering::SeqCst) {
        report(Report::End { played });
    }
}

#[cfg(test)]
mod tests {
    use super::{out_size, Info};

    #[test]
    fn sizes_fit_and_stay_even() {
        let portrait = Info {
            width: 1080,
            height: 1920,
            fps: 30.0,
            duration: 1.0,
        };
        assert_eq!(out_size(&portrait), (404, 720));
        let small = Info {
            width: 321,
            height: 241,
            fps: 30.0,
            duration: 1.0,
        };
        assert_eq!(out_size(&small), (320, 240));
    }
}
