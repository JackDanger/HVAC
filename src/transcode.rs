use anyhow::{bail, Context, Result};
use indicatif::ProgressBar;
use std::io::BufRead;
use std::os::unix::fs::{chown, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::config::TargetConfig;
use crate::gpu::{GpuInfo, GpuKind};
use crate::probe;

/// Guard that kills the ffmpeg child process on drop (prevents orphans).
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Determine the output path for a transcoded file.
/// If `output_dir` is Some, place the file there preserving the filename.
/// Otherwise, place it next to the original with a `.transcoded.{container}` extension.
pub fn output_path(source: &Path, output_dir: Option<&Path>, container: &str) -> Result<PathBuf> {
    let stem = source
        .file_stem()
        .context("source file has no stem")?
        .to_string_lossy();

    if let Some(dir) = output_dir {
        std::fs::create_dir_all(dir)?;
        Ok(dir.join(format!("{}.{}", stem, container)))
    } else {
        let parent = source.parent().context("source has no parent directory")?;
        Ok(parent.join(format!("{}.transcoded.{}", stem, container)))
    }
}

/// Check if an existing output file is a valid, complete transcode of the source.
/// Used for resume: if output already exists and passes validation, skip re-encoding.
pub fn output_already_valid(output: &Path, source: &Path, source_duration_secs: f64) -> bool {
    if !output.exists() {
        return false;
    }
    validate_output(output, source, source_duration_secs).is_ok()
}

/// Transcode a file using ffmpeg with GPU acceleration.
/// If `output` is None, transcode in-place (to a temp file, then replace original).
/// `source_bitrate_kbps` is used to cap the output so we never produce a larger file.
/// `source_pix_fmt` is used to handle 10-bit content that needs pixel format conversion.
/// If `progress` is provided, ffmpeg progress is parsed and the bar is updated in real time.
#[allow(clippy::too_many_arguments)]
pub fn transcode(
    source: &Path,
    output: Option<&Path>,
    target: &TargetConfig,
    gpu: &GpuInfo,
    source_bitrate_kbps: u32,
    source_duration_secs: f64,
    source_pix_fmt: &str,
    progress: Option<&ProgressBar>,
) -> Result<PathBuf> {
    let final_output = match output {
        Some(p) => p.to_path_buf(),
        None => {
            // In-place: use temp file then rename
            let parent = source.parent().context("source has no parent")?;
            parent.join(format!(
                ".tdorr_tmp_{}.{}",
                source.file_stem().unwrap_or_default().to_string_lossy(),
                target.container
            ))
        }
    };

    let source_size = std::fs::metadata(source).map(|m| m.len()).unwrap_or(0);

    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-y"]);

    // Input
    cmd.args(["-i"]).arg(source);

    let is_10bit = probe::is_10bit(source_pix_fmt);

    // Video encoding with GPU
    match gpu.kind {
        GpuKind::Nvidia => {
            cmd.args(["-c:v", "hevc_nvenc"]);
            // Map NVENC preset names: slow -> p7, medium -> p4
            let nvenc_preset = match target.preset.as_str() {
                "slow" | "slower" | "veryslow" => "p7",
                "medium" => "p4",
                "fast" | "faster" | "veryfast" => "p1",
                other => other,
            };
            cmd.args(["-preset", nvenc_preset]);
            cmd.args(["-rc", "vbr"]);
            cmd.args(["-cq", &target.quality.to_string()]);
            // Cap at source bitrate so we never produce a larger file.
            if source_bitrate_kbps > 0 {
                cmd.args(["-maxrate", &format!("{}k", source_bitrate_kbps)]);
                cmd.args(["-bufsize", &format!("{}k", source_bitrate_kbps * 2)]);
            }
            cmd.args(["-b:v", "0"]);
            // Handle 10-bit: convert to p010le for NVENC 10-bit support
            if is_10bit {
                cmd.args(["-pix_fmt", "p010le"]);
            }
        }
        GpuKind::Intel => {
            cmd.args(["-vaapi_device", "/dev/dri/renderD128"]);
            cmd.args(["-c:v", "hevc_vaapi"]);
            cmd.args(["-global_quality", &target.quality.to_string()]);
            if source_bitrate_kbps > 0 {
                cmd.args(["-maxrate", &format!("{}k", source_bitrate_kbps)]);
                cmd.args(["-bufsize", &format!("{}k", source_bitrate_kbps * 2)]);
            }
            if is_10bit {
                cmd.args(["-vf", "format=p010le,hwupload"]);
            } else {
                cmd.args(["-vf", "format=nv12,hwupload"]);
            }
        }
        GpuKind::Apple => {
            cmd.args(["-c:v", "hevc_videotoolbox"]);
            cmd.args(["-q:v", &target.quality.to_string()]);
            if source_bitrate_kbps > 0 {
                cmd.args(["-maxrate", &format!("{}k", source_bitrate_kbps)]);
                cmd.args(["-bufsize", &format!("{}k", source_bitrate_kbps * 2)]);
            }
        }
    }

    // Audio
    cmd.args(["-c:a", &target.audio_codec]);

    // Subtitles
    if target.subtitle_codec == "copy" {
        cmd.args(["-c:s", "copy"]);
    }

    // Map video, audio, and subtitle streams — skip attached pics (cover art)
    // which can be too small for GPU encoders and cause failures
    cmd.args(["-map", "0:v:0"]); // first video stream only
    cmd.args(["-map", "0:a?"]); // all audio streams
    cmd.args(["-map", "0:s?"]); // all subtitle streams

    // Progress reporting
    if progress.is_some() {
        cmd.args(["-progress", "pipe:1", "-nostats"]);
    }

    // Output
    cmd.arg(&final_output);

    log::info!("Running: {:?}", cmd);

    if let Some(bar) = progress {
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut guard = ChildGuard(cmd.spawn().context("Failed to execute ffmpeg")?);
        let stdout = guard.0.stdout.take().unwrap();
        let stderr = guard.0.stderr.take().unwrap();

        // Drain stderr in background to avoid deadlock
        let stderr_handle = std::thread::spawn(move || {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::BufReader::new(stderr), &mut buf).ok();
            buf
        });

        // Parse ffmpeg progress from stdout
        let reader = std::io::BufReader::new(stdout);
        let duration_us = (source_duration_secs * 1_000_000.0) as i64;

        for line in reader.lines().map_while(Result::ok) {
            if let Some(time_str) = line.strip_prefix("out_time_us=") {
                if let Ok(us) = time_str.parse::<i64>() {
                    if duration_us > 0 && us > 0 {
                        let pos =
                            ((us as f64 / duration_us as f64) * 1000.0).clamp(0.0, 1000.0) as u64;
                        bar.set_position(pos);
                    }
                }
            } else if let Some(speed_str) = line.strip_prefix("speed=") {
                let speed = speed_str.trim().trim_end_matches('x');
                if speed != "N/A" && !speed.is_empty() {
                    bar.set_prefix(format!("{speed}x"));
                }
            }
        }

        let status = guard.0.wait().context("Failed to wait for ffmpeg")?;
        let stderr_output = stderr_handle.join().unwrap_or_default();

        // Disarm the guard — process has already exited
        std::mem::forget(guard);

        if !status.success() {
            let _ = std::fs::remove_file(&final_output);
            let last_line = stderr_output.lines().last().unwrap_or("unknown error");
            bail!("ffmpeg exited with status {}: {}", status, last_line);
        }
    } else {
        let status = cmd.status().context("Failed to execute ffmpeg")?;

        if !status.success() {
            let _ = std::fs::remove_file(&final_output);
            bail!("ffmpeg exited with status: {}", status);
        }
    }

    // Validate the output before considering it done
    if let Err(e) = validate_output(&final_output, source, source_duration_secs) {
        let _ = std::fs::remove_file(&final_output);
        bail!("Output validation failed: {}", e);
    }

    // Copy permissions (user, group, mode) from source to output
    copy_permissions(source, &final_output)?;

    // Report size savings
    let output_size = std::fs::metadata(&final_output)
        .map(|m| m.len())
        .unwrap_or(0);
    if source_size > 0 && output_size > 0 {
        let saved = source_size as i64 - output_size as i64;
        let pct = (saved as f64 / source_size as f64) * 100.0;
        log::info!(
            "Size: {} -> {} ({:+.1}%)",
            format_size(source_size),
            format_size(output_size),
            -pct,
        );
    }

    // If in-place mode, replace original only after validation passes
    if output.is_none() {
        std::fs::rename(&final_output, source)
            .context("Failed to replace original file with transcoded version")?;
        return Ok(source.to_path_buf());
    }

    Ok(final_output)
}

/// Return the size of the output file, or 0 if it doesn't exist.
pub fn output_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1}GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else {
        format!("{:.0}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Copy file permissions (owner, group, mode) from source to destination.
fn copy_permissions(source: &Path, dest: &Path) -> Result<()> {
    let src_meta = std::fs::metadata(source).context("Failed to read source metadata")?;

    // Copy mode (chmod)
    std::fs::set_permissions(dest, src_meta.permissions())
        .context("Failed to set file permissions")?;

    // Copy owner and group (chown) - may fail if not running as root
    let uid = src_meta.uid();
    let gid = src_meta.gid();
    if let Err(e) = chown(dest, Some(uid), Some(gid)) {
        log::warn!(
            "Could not set owner/group on {:?}: {} (requires root)",
            dest,
            e
        );
    }

    Ok(())
}

/// Validate transcoded output to prevent corruption.
/// Checks:
/// 1. Output file exists and is non-empty
/// 2. Output is at least 1% the size of the source (catches truncated files)
/// 3. ffprobe can read it and finds a video stream
/// 4. Duration is within 5 seconds of the source (catches incomplete transcodes)
fn validate_output(output: &Path, source: &Path, source_duration_secs: f64) -> Result<()> {
    let out_meta = std::fs::metadata(output).context("Output file does not exist")?;
    if out_meta.len() == 0 {
        bail!("Output file is empty");
    }

    let src_meta = std::fs::metadata(source).context("Source file disappeared")?;
    if out_meta.len() < src_meta.len() / 100 {
        bail!(
            "Output file is suspiciously small ({} bytes vs {} bytes source)",
            out_meta.len(),
            src_meta.len()
        );
    }

    let out_info = probe::probe_file(output).context("ffprobe cannot read output file")?;

    if out_info.codec == "unknown" {
        bail!("Output has no recognizable video codec");
    }

    if source_duration_secs > 0.0 && out_info.duration_secs > 0.0 {
        let diff = (source_duration_secs - out_info.duration_secs).abs();
        if diff > 5.0 {
            bail!(
                "Duration mismatch: source {:.1}s vs output {:.1}s (diff {:.1}s)",
                source_duration_secs,
                out_info.duration_secs,
                diff
            );
        }
    }

    log::info!(
        "Validation passed: {} codec, {:.1}s duration, {} bytes",
        out_info.codec,
        out_info.duration_secs,
        out_meta.len()
    );

    Ok(())
}

/// Check if an ffmpeg error looks like an NVENC session limit issue.
pub fn is_session_limit_error(error_msg: &str) -> bool {
    error_msg.contains("out of memory")
        || error_msg.contains("InitializeEncoder failed")
        || error_msg.contains("Cannot init NVENC")
        || error_msg.contains("exit status: 69")
}

/// Replace an original file with its transcoded copy.
/// Validates the transcoded file first, then atomically replaces.
pub fn replace_original(
    original: &Path,
    transcoded: &Path,
    source_duration_secs: f64,
) -> Result<u64> {
    if !transcoded.exists() {
        bail!("Transcoded file does not exist: {:?}", transcoded);
    }

    // Validate again before replacing
    validate_output(transcoded, original, source_duration_secs)?;

    let original_size = std::fs::metadata(original).map(|m| m.len()).unwrap_or(0);
    let transcoded_size = std::fs::metadata(transcoded).map(|m| m.len()).unwrap_or(0);

    // Copy permissions before replacing
    copy_permissions(original, transcoded)?;

    std::fs::rename(transcoded, original)
        .with_context(|| format!("Failed to replace {:?} with transcoded version", original))?;

    Ok(original_size.saturating_sub(transcoded_size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_output_path_next_to_source() {
        let source = Path::new("/mnt/media/show/episode.mkv");
        let result = output_path(source, None, "mkv").unwrap();
        assert_eq!(
            result,
            PathBuf::from("/mnt/media/show/episode.transcoded.mkv")
        );
    }

    #[test]
    fn test_output_path_custom_dir() {
        let dir = tempfile::tempdir().unwrap();
        let source = Path::new("/mnt/media/show/episode.mkv");
        let result = output_path(source, Some(dir.path()), "mkv").unwrap();
        assert_eq!(result, dir.path().join("episode.mkv"));
    }

    #[test]
    fn test_output_path_different_container() {
        let source = Path::new("/mnt/media/show/episode.avi");
        let result = output_path(source, None, "mkv").unwrap();
        assert_eq!(
            result,
            PathBuf::from("/mnt/media/show/episode.transcoded.mkv")
        );
    }

    #[test]
    fn test_is_session_limit_error() {
        assert!(is_session_limit_error(
            "ffmpeg exited with status exit status: 69"
        ));
        assert!(is_session_limit_error("Cannot init NVENC encoder"));
        assert!(!is_session_limit_error("some other error"));
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(1024 * 1024), "1MB");
        assert_eq!(format_size(500 * 1024 * 1024), "500MB");
        assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2.0GB");
    }
}
