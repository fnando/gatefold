use anyhow::{Context, Result, anyhow, bail};
use std::path::Path;
use std::process::Command;

/// Confirm `ffprobe` is callable, failing early when `--verify` is requested
/// but ffmpeg isn't installed.
pub fn ensure_available() -> Result<()> {
    Command::new("ffprobe")
        .arg("-version")
        .output()
        .map(|_| ())
        .map_err(|_| anyhow!("--verify requires `ffprobe` (from ffmpeg) on your PATH"))
}

/// Validate an audio file by probing it with ffprobe. Returns an error when the
/// container/streams don't parse cleanly.
pub fn audio(path: &Path) -> Result<()> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .context("running ffprobe")?;

    if !output.status.success() || !output.stderr.is_empty() {
        let message = String::from_utf8_lossy(&output.stderr);
        bail!(
            "ffprobe reported the written file is invalid: {}",
            message.trim()
        );
    }
    Ok(())
}
