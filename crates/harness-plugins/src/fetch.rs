//! Downloads with the `git` CLI: a shallow fetch of one revision into a folder, swapped in atomically.
//!
//! Blocking (runs `git` and waits); call it from `spawn_blocking`. Git never prompts (no terminal, no
//! credential UI), so a private or missing repository fails instead of hanging.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{PluginError, Result};
use crate::marketplace::{check_rev, check_url};

/// Longest one git step may run.
const STEP_TIMEOUT: Duration = Duration::from_secs(300);

/// Makes `dest` hold `url` at `rev` (default branch when `None`) and returns the checked-out commit.
///
/// When `dest` is already a checkout of the same full sha nothing is downloaded.
///
/// # Errors
///
/// Unsafe url or revision, `git` missing or failing, I/O.
pub fn checkout(url: &str, rev: Option<&str>, dest: &Path) -> Result<String> {
    check_url(url)?;
    if let Some(rev) = rev {
        check_rev(rev)?;
        if is_full_sha(rev) && head(dest).is_some_and(|h| h.eq_ignore_ascii_case(rev)) {
            return Ok(rev.to_owned());
        }
    }
    let parent = dest
        .parent()
        .ok_or_else(|| PluginError::invalid("download folder has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = parent.join(format!(".{name}.download-{}", std::process::id()));
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp)?;
    }
    std::fs::create_dir_all(&tmp)?;
    let result = (|| {
        git(&tmp, &["init", "-q"])?;
        git(
            &tmp,
            &[
                "fetch",
                "-q",
                "--depth",
                "1",
                "--no-tags",
                "--",
                url,
                rev.unwrap_or("HEAD"),
            ],
        )?;
        git(
            &tmp,
            &[
                "-c",
                "advice.detachedHead=false",
                "checkout",
                "-q",
                "--detach",
                "FETCH_HEAD",
            ],
        )?;
        head(&tmp).ok_or_else(|| PluginError::Fetch("no commit after checkout".into()))
    })();
    let sha = match result {
        Ok(sha) => sha,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e);
        }
    };
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    std::fs::rename(&tmp, dest)?;
    Ok(sha)
}

/// The commit checked out in `dir`, if it is a git checkout.
#[must_use]
pub fn head(dir: &Path) -> Option<String> {
    if !dir.join(".git").exists() {
        return None;
    }
    run(dir, &["rev-parse", "HEAD"])
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| is_full_sha(s))
}

fn is_full_sha(rev: &str) -> bool {
    rev.len() == 40 && rev.chars().all(|c| c.is_ascii_hexdigit())
}

fn git(dir: &Path, args: &[&str]) -> Result<()> {
    run(dir, args).map(|_| ())
}

fn run(dir: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args([
        "-c",
        "protocol.allow=never",
        "-c",
        "protocol.https.allow=always",
        "-c",
        "core.hooksPath=",
    ])
    .args(args)
    .current_dir(dir)
    .env("GIT_TERMINAL_PROMPT", "0")
    .env("GCM_INTERACTIVE", "never")
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：下载插件时不弹出 git 控制台窗口。
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| PluginError::Fetch(format!("cannot run git (is it installed?): {e}")))?;
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if started.elapsed() > STEP_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(PluginError::Fetch(format!("git {} timed out", args[0])));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let out = child.wait_with_output()?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("failed");
        Err(PluginError::Fetch(format!(
            "git {}: {}",
            args[0],
            line.trim()
        )))
    }
}
