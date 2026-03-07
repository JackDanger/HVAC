use anyhow::{bail, Context, Result};
use std::io::BufRead;
use std::os::unix::fs::{chown, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::TargetConfig;
use crate::gpu::{GpuInfo, GpuKind};
use crate::probe;
use crate::util::format_size;

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
/// If `progress` is provided, it's updated with 0-1000 as encoding progresses.
#[allow(clippy::too_many_arguments)]
pub fn transcode(
    source: &Path,
    output: Option<&Path>,
    target: &TargetConfig,
    gpu: &GpuInfo,
    source_bitrate_kbps: u32,
    source_duration_secs: f64,
    source_pix_fmt: &str,
    progress: Option<&AtomicU64>,
    speed: Option<&AtomicU64>,
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
            let nvenc_preset = match target.preset.as_str() {
                "slow" | "slower" | "veryslow" => "p7",
                "medium" => "p4",
                "fast" | "faster" | "veryfast" => "p1",
                other => other,
            };
            cmd.args(["-preset", nvenc_preset]);
            cmd.args(["-rc", "vbr"]);
            cmd.args(["-cq", &target.quality.to_string()]);
            if source_bitrate_kbps > 0 {
                cmd.args(["-maxrate", &format!("{}k", source_bitrate_kbps)]);
                cmd.args(["-bufsize", &format!("{}k", source_bitrate_kbps * 2)]);
            }
            cmd.args(["-b:v", "0"]);
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
    cmd.args(["-map", "0:v:0"]);
    cmd.args(["-map", "0:a?"]);
    cmd.args(["-map", "0:s?"]);

    // Progress reporting via stdout if caller wants it
    if progress.is_some() {
        cmd.args(["-progress", "pipe:1", "-nostats"]);
    }

    // Output
    cmd.arg(&final_output);

    log::debug!("Running: {:?}", cmd);

    // Pipe stderr always; pipe stdout only if tracking progress
    cmd.stderr(Stdio::piped());
    if progress.is_some() {
        cmd.stdout(Stdio::piped());
    } else {
        cmd.stdout(Stdio::null());
    }

    let mut guard = ChildGuard(cmd.spawn().context("Failed to execute ffmpeg")?);
    let stderr = guard.0.stderr.take().unwrap();

    // Drain stderr in background
    let stderr_handle = std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stderr);
        let mut buf = String::new();
        for line in reader.lines().map_while(Result::ok) {
            buf.push_str(&line);
            buf.push('\n');
        }
        buf
    });

    // Parse ffmpeg progress from stdout if tracking
    if let Some(prog) = progress {
        let stdout = guard.0.stdout.take().unwrap();
        let reader = std::io::BufReader::new(stdout);
        let duration_us = (source_duration_secs * 1_000_000.0) as i64;

        for line in reader.lines().map_while(Result::ok) {
            if let Some(time_str) = line.strip_prefix("out_time_us=") {
                if let Ok(us) = time_str.parse::<i64>() {
                    if duration_us > 0 && us > 0 {
                        let pos =
                            ((us as f64 / duration_us as f64) * 1000.0).clamp(0.0, 1000.0) as u64;
                        prog.store(pos, Ordering::Relaxed);
                    }
                }
            } else if let Some(speed_str) = line.strip_prefix("speed=") {
                if let Some(spd) = speed {
                    // speed looks like "1.23x" or "N/A"
                    let trimmed = speed_str.trim_end_matches('x');
                    if let Ok(v) = trimmed.parse::<f64>() {
                        spd.store((v * 100.0) as u64, Ordering::Relaxed);
                    }
                }
            }
        }
    }

    let status = guard.0.wait().context("Failed to wait for ffmpeg")?;
    let stderr_output = stderr_handle.join().unwrap_or_default();

    // Disarm the guard — process has already exited
    std::mem::forget(guard);

    if !status.success() {
        let _ = std::fs::remove_file(&final_output);
        let context = last_n_lines(&stderr_output, 3);
        bail!("ffmpeg failed ({}): {}", status, context);
    }

    // Validate the output before considering it done
    if let Err(e) = validate_output(&final_output, source, source_duration_secs) {
        let _ = std::fs::remove_file(&final_output);
        bail!("Output validation failed: {}", e);
    }

    // Copy permissions (user, group, mode) from source to output
    copy_permissions(source, &final_output)?;

    // Report size savings at debug level
    let output_size = std::fs::metadata(&final_output)
        .map(|m| m.len())
        .unwrap_or(0);
    if source_size > 0 && output_size > 0 {
        let saved = source_size as i64 - output_size as i64;
        let pct = (saved as f64 / source_size as f64) * 100.0;
        log::debug!(
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

/// Transcode file(s) from inside an ISO by streaming them to ffmpeg via stdin.
/// Multiple inner paths are concatenated sequentially (e.g. Blu-ray chapters).
/// The ISO contents are piped directly to ffmpeg without extracting to disk.
#[allow(clippy::too_many_arguments)]
pub fn transcode_iso(
    iso_path: &Path,
    inner_paths: &[String],
    output: &Path,
    target: &TargetConfig,
    gpu: &GpuInfo,
    source_bitrate_kbps: u32,
    source_duration_secs: f64,
    source_pix_fmt: &str,
    progress: Option<&AtomicU64>,
    speed: Option<&AtomicU64>,
) -> Result<PathBuf> {
    let final_output = output.to_path_buf();

    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-y"]);

    // Input from stdin
    cmd.args(["-i", "pipe:0"]);

    let is_10bit = probe::is_10bit(source_pix_fmt);

    // Video encoding with GPU (same as transcode())
    match gpu.kind {
        GpuKind::Nvidia => {
            cmd.args(["-c:v", "hevc_nvenc"]);
            let nvenc_preset = match target.preset.as_str() {
                "slow" | "slower" | "veryslow" => "p7",
                "medium" => "p4",
                "fast" | "faster" | "veryfast" => "p1",
                other => other,
            };
            cmd.args(["-preset", nvenc_preset]);
            cmd.args(["-rc", "vbr"]);
            cmd.args(["-cq", &target.quality.to_string()]);
            if source_bitrate_kbps > 0 {
                cmd.args(["-maxrate", &format!("{}k", source_bitrate_kbps)]);
                cmd.args(["-bufsize", &format!("{}k", source_bitrate_kbps * 2)]);
            }
            cmd.args(["-b:v", "0"]);
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

    // Map streams
    cmd.args(["-map", "0:v:0"]);
    cmd.args(["-map", "0:a?"]);
    cmd.args(["-map", "0:s?"]);

    // Progress reporting
    if progress.is_some() {
        cmd.args(["-progress", "pipe:1", "-nostats"]);
    }

    // Output
    cmd.arg(&final_output);

    log::debug!("Running (piped from ISO): {:?}", cmd);

    cmd.stderr(Stdio::piped());
    cmd.stdin(Stdio::piped());
    if progress.is_some() {
        cmd.stdout(Stdio::piped());
    } else {
        cmd.stdout(Stdio::null());
    }

    let mut guard = ChildGuard(cmd.spawn().context("Failed to execute ffmpeg")?);
    let stderr = guard.0.stderr.take().unwrap();
    let stdin = guard.0.stdin.take().unwrap();

    // Stream ISO contents to ffmpeg stdin in a background thread
    let iso = iso_path.to_path_buf();
    let paths = inner_paths.to_vec();
    let stdin_handle = std::thread::spawn(move || {
        let mut stdin = stdin;
        let _ = crate::iso::cat_files(&iso, &paths, &mut stdin);
        // Drop stdin to signal EOF to ffmpeg
    });

    // Drain stderr in background
    let stderr_handle = std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stderr);
        let mut buf = String::new();
        for line in reader.lines().map_while(Result::ok) {
            buf.push_str(&line);
            buf.push('\n');
        }
        buf
    });

    // Parse progress from stdout
    if let Some(prog) = progress {
        let stdout = guard.0.stdout.take().unwrap();
        let reader = std::io::BufReader::new(stdout);
        let duration_us = (source_duration_secs * 1_000_000.0) as i64;

        for line in reader.lines().map_while(Result::ok) {
            if let Some(time_str) = line.strip_prefix("out_time_us=") {
                if let Ok(us) = time_str.parse::<i64>() {
                    if duration_us > 0 && us > 0 {
                        let pos =
                            ((us as f64 / duration_us as f64) * 1000.0).clamp(0.0, 1000.0) as u64;
                        prog.store(pos, Ordering::Relaxed);
                    }
                }
            } else if let Some(speed_str) = line.strip_prefix("speed=") {
                if let Some(spd) = speed {
                    let trimmed = speed_str.trim_end_matches('x');
                    if let Ok(v) = trimmed.parse::<f64>() {
                        spd.store((v * 100.0) as u64, Ordering::Relaxed);
                    }
                }
            }
        }
    }

    let status = guard.0.wait().context("Failed to wait for ffmpeg")?;
    let stderr_output = stderr_handle.join().unwrap_or_default();
    let _ = stdin_handle.join();

    // Disarm the guard
    std::mem::forget(guard);

    if !status.success() {
        let _ = std::fs::remove_file(&final_output);
        let context = last_n_lines(&stderr_output, 3);
        bail!("ffmpeg failed ({}): {}", status, context);
    }

    // Validate output (can't compare to source size for ISO streams,
    // just check it's non-empty and has valid duration)
    let out_meta = std::fs::metadata(&final_output).context("Output file does not exist")?;
    if out_meta.len() == 0 {
        let _ = std::fs::remove_file(&final_output);
        bail!("Output file is empty");
    }

    let out_info = probe::probe_file(&final_output).context("ffprobe cannot read output file")?;
    if out_info.codec == "unknown" {
        let _ = std::fs::remove_file(&final_output);
        bail!("Output has no recognizable video codec");
    }

    if source_duration_secs > 0.0 && out_info.duration_secs > 0.0 {
        let diff = (source_duration_secs - out_info.duration_secs).abs();
        if diff > 5.0 {
            let _ = std::fs::remove_file(&final_output);
            bail!(
                "Duration mismatch: source {:.1}s vs output {:.1}s (diff {:.1}s)",
                source_duration_secs,
                out_info.duration_secs,
                diff
            );
        }
    }

    log::debug!(
        "ISO transcode done: {} codec, {:.1}s, {} bytes",
        out_info.codec,
        out_info.duration_secs,
        out_meta.len()
    );

    Ok(final_output)
}

/// Return the size of the output file, or 0 if it doesn't exist.
pub fn output_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Copy file permissions (owner, group, mode) from source to destination.
fn copy_permissions(source: &Path, dest: &Path) -> Result<()> {
    let src_meta = std::fs::metadata(source).context("Failed to read source metadata")?;

    std::fs::set_permissions(dest, src_meta.permissions())
        .context("Failed to set file permissions")?;

    let uid = src_meta.uid();
    let gid = src_meta.gid();
    if let Err(e) = chown(dest, Some(uid), Some(gid)) {
        log::debug!("Could not chown {:?}: {} (requires root)", dest, e);
    }

    Ok(())
}

/// Validate transcoded output to prevent corruption.
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

    log::debug!(
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
        || error_msg.contains("OpenEncodeSessionEx failed")
        || error_msg.contains("No capable devices found")
        || error_msg.contains("Nothing was written into output file")
        || error_msg.contains("exit status: 69")
        || error_msg.contains("exit status: 187")
}

/// Check if an ffmpeg error is a disk space issue.
pub fn is_disk_space_error(error_msg: &str) -> bool {
    error_msg.contains("Disk quota exceeded")
        || error_msg.contains("No space left on device")
        || error_msg.contains("ENOSPC")
}

/// Extract the last N non-empty lines from a string, joined by " | ".
fn last_n_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    let result = lines[start..].join(" | ");
    if result.is_empty() {
        "unknown error".to_string()
    } else {
        result
    }
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

    validate_output(transcoded, original, source_duration_secs)?;

    let original_size = std::fs::metadata(original).map(|m| m.len()).unwrap_or(0);
    let transcoded_size = std::fs::metadata(transcoded).map(|m| m.len()).unwrap_or(0);

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
        assert!(is_session_limit_error(
            "ffmpeg failed (exit status: 187): Nothing was written into output file"
        ));
        assert!(!is_session_limit_error("some other error"));
    }

    #[test]
    fn test_is_disk_space_error() {
        assert!(is_disk_space_error(
            "Error opening output file: Disk quota exceeded"
        ));
        assert!(is_disk_space_error("No space left on device"));
        assert!(!is_disk_space_error("some other ffmpeg error"));
    }

    #[test]
    fn test_last_n_lines() {
        let s = "line1\nline2\nline3\nline4\n";
        assert_eq!(last_n_lines(s, 2), "line3 | line4");
        assert_eq!(last_n_lines("", 3), "unknown error");
        assert_eq!(last_n_lines("only\n", 5), "only");
    }
}
