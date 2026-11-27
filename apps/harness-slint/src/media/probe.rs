//! ffmpeg discovery and one-shot jobs: video facts (size, frame rate, duration) and a preview frame.
//! ffmpeg is the user's own install (PATH); without it videos still open with the default app.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use slint::{Rgba8Pixel, SharedPixelBuffer};

/// `CREATE_NO_WINDOW` (winbase.h): ffmpeg is a console program started from a GUI app.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn find(name: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(&exe))
        .find(|candidate| candidate.is_file())
}

/// `ffmpeg`, `ffprobe` and `ffplay` from PATH, looked up once.
pub(crate) fn tool(name: &'static str) -> Option<&'static Path> {
    static FFMPEG: OnceLock<Option<PathBuf>> = OnceLock::new();
    static FFPROBE: OnceLock<Option<PathBuf>> = OnceLock::new();
    static FFPLAY: OnceLock<Option<PathBuf>> = OnceLock::new();
    let cell = match name {
        "ffmpeg" => &FFMPEG,
        "ffprobe" => &FFPROBE,
        _ => &FFPLAY,
    };
    cell.get_or_init(|| find(name)).as_deref()
}

/// A command for an ffmpeg tool without a console window and without stdin.
pub(crate) fn command(program: &Path) -> Command {
    let mut command = Command::new(program);
    command.stdin(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// What the player needs to know about a video.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Info {
    /// Displayed size (after the rotation phones record).
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// Seconds; 0 when unknown.
    pub duration: f64,
}

fn rate(text: &str) -> Option<f64> {
    let (num, den) = text.split_once('/').unwrap_or((text, "1"));
    let (num, den) = (
        num.trim().parse::<f64>().ok()?,
        den.trim().parse::<f64>().ok()?,
    );
    (num > 0.0 && den > 0.0).then(|| num / den)
}

/// Parses `ffprobe -of json` output (first video stream + format duration).
pub(crate) fn parse_info(json: &str) -> Option<Info> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let stream = value.get("streams")?.as_array()?.first()?;
    let mut width = u32::try_from(stream.get("width")?.as_u64()?).ok()?;
    let mut height = u32::try_from(stream.get("height")?.as_u64()?).ok()?;
    let rotation = stream
        .get("side_data_list")
        .and_then(|list| list.as_array())
        .and_then(|list| list.iter().find_map(|d| d.get("rotation")?.as_f64()))
        .or_else(|| {
            stream
                .get("tags")?
                .get("rotate")?
                .as_str()?
                .parse::<f64>()
                .ok()
        })
        .unwrap_or(0.0);
    // 修复：手机竖拍视频的宽高是旋转前的值，ffmpeg 解码时会自动旋转，所以 90/270 度要交换。
    if (rotation.abs() % 180.0 - 90.0).abs() < 0.5 {
        std::mem::swap(&mut width, &mut height);
    }
    let fps = ["avg_frame_rate", "r_frame_rate"]
        .iter()
        .find_map(|key| rate(stream.get(*key)?.as_str()?))
        .filter(|fps| *fps <= 240.0)
        .unwrap_or(30.0);
    let duration = value
        .get("format")
        .and_then(|f| f.get("duration"))
        .and_then(|d| d.as_str())
        .and_then(|d| d.parse::<f64>().ok())
        .unwrap_or(0.0);
    (width > 0 && height > 0).then_some(Info {
        width,
        height,
        fps,
        duration,
    })
}

pub(crate) fn info(path: &Path) -> Option<Info> {
    let output = command(tool("ffprobe")?)
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
        .arg(
            "stream=width,height,avg_frame_rate,r_frame_rate:stream_tags=rotate:\
             stream_side_data=rotation:format=duration",
        )
        .args(["-of", "json"])
        .arg(path)
        .stdout(Stdio::piped())
        .output()
        .ok()?;
    parse_info(&String::from_utf8_lossy(&output.stdout))
}

/// A representative frame of a video, at most `width` x `height`.
pub fn video_thumb(path: &Path, width: u32, height: u32) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let filter =
        format!("thumbnail=24,scale={width}:{height}:force_original_aspect_ratio=decrease");
    let output = command(tool("ffmpeg")?)
        .args(["-v", "error", "-i"])
        .arg(path)
        .args([
            "-frames:v",
            "1",
            "-vf",
            &filter,
            "-f",
            "image2pipe",
            "-vcodec",
            "png",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .output()
        .ok()?;
    let image = image::load_from_memory(&output.stdout).ok()?.to_rgba8();
    Some(SharedPixelBuffer::clone_from_slice(
        image.as_raw(),
        image.width(),
        image.height(),
    ))
}

/// Plain dark 16:9 tile for a video whose frame cannot be read.
pub fn placeholder() -> SharedPixelBuffer<Rgba8Pixel> {
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(160, 90);
    for pixel in buffer.make_mut_slice() {
        *pixel = Rgba8Pixel {
            r: 0x26,
            g: 0x28,
            b: 0x2c,
            a: 0xff,
        };
    }
    buffer
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::parse_info;

    #[test]
    fn rotated_phone_video_swaps_size() {
        let json = r#"{"streams":[{"width":1920,"height":1080,"avg_frame_rate":"30000/1001",
            "side_data_list":[{"rotation":-90}]}],"format":{"duration":"15.10"}}"#;
        let info = parse_info(json).unwrap();
        assert_eq!((info.width, info.height), (1080, 1920));
        assert!((info.fps - 29.97).abs() < 0.01);
        assert!((info.duration - 15.1).abs() < 0.001);
    }

    #[test]
    fn missing_rate_falls_back() {
        let json = r#"{"streams":[{"width":640,"height":360,"avg_frame_rate":"0/0"}],"format":{}}"#;
        let info = parse_info(json).unwrap();
        assert!((info.fps - 30.0).abs() < f64::EPSILON);
        assert!(info.duration.abs() < f64::EPSILON);
    }
}
