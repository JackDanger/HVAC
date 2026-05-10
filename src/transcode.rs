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
/// If `force_reencode_audio` is true, the configured `audio_codec: copy` is overridden
/// with `aac` — used to retry after a copy-incompatible codec (e.g. pcm_dvd → MKV) fails.
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
    skip_subs: bool,
    force_reencode_audio: bool,
) -> Result<PathBuf> {
    let final_output = match output {
        Some(p) => p.to_path_buf(),
        None => {
            // In-place: use temp file then rename
            let parent = source.parent().context("source has no parent")?;
            parent.join(format!(
                ".hvac_tmp_{}.{}",
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
    let audio_codec = if force_reencode_audio && target.audio_codec == "copy" {
        AUDIO_REENCODE_FALLBACK
    } else {
        target.audio_codec.as_str()
    };
    cmd.args(["-c:a", audio_codec]);

    // Subtitles
    if !skip_subs && target.subtitle_codec == "copy" {
        cmd.args(["-c:s", "copy"]);
    }

    // Map video, audio, and subtitle streams — skip attached pics (cover art)
    cmd.args(["-map", "0:v:0"]);
    cmd.args(["-map", "0:a?"]);
    if !skip_subs {
        cmd.args(["-map", "0:s?"]);
    }

    // Progress reporting via stdout if caller wants it
    if progress.is_some() {
        cmd.args(["-progress", "pipe:1", "-nostats"]);
    }

    // Output
    cmd.arg(&final_output);

    log::debug!(
        "Running{}{}: {:?}",
        if skip_subs { " (no subs)" } else { "" },
        if force_reencode_audio {
            " (audio re-encode)"
        } else {
            ""
        },
        cmd
    );

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
            // io::BufRead::lines() splits on \n/\r\n but NOT bare \r.
            // ffmpeg writes progress updates with \r; split on those too so
            // last_n_lines() sees clean individual lines instead of one garbled blob.
            for part in line.split('\r') {
                let trimmed = part.trim_end();
                if !trimmed.is_empty() {
                    buf.push_str(trimmed);
                    buf.push('\n');
                }
            }
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
        let context = summarize_ffmpeg_error(&stderr_output);
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
/// `force_reencode_audio` overrides `audio_codec: copy` with `aac` for retries
/// after a copy-incompatible codec is detected.
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
    skip_subs: bool,
    force_reencode_audio: bool,
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
    let audio_codec = if force_reencode_audio && target.audio_codec == "copy" {
        AUDIO_REENCODE_FALLBACK
    } else {
        target.audio_codec.as_str()
    };
    cmd.args(["-c:a", audio_codec]);

    // Subtitles
    if !skip_subs && target.subtitle_codec == "copy" {
        cmd.args(["-c:s", "copy"]);
    }

    // Map streams
    cmd.args(["-map", "0:v:0"]);
    cmd.args(["-map", "0:a?"]);
    if !skip_subs {
        cmd.args(["-map", "0:s?"]);
    }

    // Progress reporting
    if progress.is_some() {
        cmd.args(["-progress", "pipe:1", "-nostats"]);
    }

    // Output
    cmd.arg(&final_output);

    log::debug!(
        "Running (piped from ISO{}{}): {:?}",
        if skip_subs { ", no subs" } else { "" },
        if force_reencode_audio {
            ", audio re-encode"
        } else {
            ""
        },
        cmd
    );

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
            for part in line.split('\r') {
                let trimmed = part.trim_end();
                if !trimmed.is_empty() {
                    buf.push_str(trimmed);
                    buf.push('\n');
                }
            }
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
        let context = summarize_ffmpeg_error(&stderr_output);
        bail!("ffmpeg failed ({}): {}", status, context);
    }

    // Validate output. We can't compare to source size for ISO streams (the
    // ISO bytes piped in are not the same as the file's effective payload),
    // so we apply the same bitrate-floor check used for direct files.
    let out_meta = std::fs::metadata(&final_output).context("Output file does not exist")?;
    if let Err(e) = check_output_size_floor(out_meta.len(), source_duration_secs) {
        let _ = std::fs::remove_file(&final_output);
        return Err(e);
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

/// Reject outputs that are too small to be a real encode for their duration.
///
/// We deliberately don't compare against source size as a ratio: heavy but
/// legitimate compression (4K HDR Blu-ray → 1080p mobile profile) can produce
/// outputs at 1-2 % of source size and would false-trip a ratio check.
/// Instead we floor at `duration × 50 kbps` (well below the smallest realistic
/// HEVC video+audio output — even speech-only AAC runs ~32 kbps), with a 64 KB
/// absolute minimum to catch trivially tiny outputs when duration is unknown
/// or very short.
///
/// Returns `Err` with a clear message when too small; never panics.
fn check_output_size_floor(out_size: u64, source_duration_secs: f64) -> Result<()> {
    if out_size == 0 {
        bail!("Output file is empty");
    }

    const MIN_KBPS_FLOOR: u64 = 50;
    const ABSOLUTE_FLOOR_BYTES: u64 = 64 * 1024;

    let expected_min_bytes =
        (source_duration_secs as u64).saturating_mul(MIN_KBPS_FLOOR * 1000 / 8);
    let floor = expected_min_bytes.max(ABSOLUTE_FLOOR_BYTES);
    if out_size < floor {
        bail!(
            "Output suspiciously small: {} bytes; expected at least {} bytes for {:.0}s duration",
            out_size,
            floor,
            source_duration_secs
        );
    }

    Ok(())
}

/// Validate transcoded output to prevent corruption.
fn validate_output(output: &Path, source: &Path, source_duration_secs: f64) -> Result<()> {
    let out_meta = std::fs::metadata(output).context("Output file does not exist")?;
    // Confirm source still exists; we don't compare sizes by ratio (see
    // check_output_size_floor below for why) but a vanished source is a
    // distinct failure worth surfacing separately.
    let _ = std::fs::metadata(source).context("Source file disappeared")?;
    check_output_size_floor(out_meta.len(), source_duration_secs)?;

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

/// Audio codec used when `audio_codec: copy` produces a stream the chosen
/// container can't accept (e.g. pcm_dvd in MKV). AAC is universally muxable.
const AUDIO_REENCODE_FALLBACK: &str = "aac";

/// Check if an ffmpeg error looks like an NVENC session limit issue.
/// Matches NVENC-specific init errors and the "Nothing was written" pattern
/// which is the most common manifestation of session exhaustion.
/// Does NOT match generic exit codes (e.g. exit 69) which can be decoder failures.
pub fn is_session_limit_error(error_msg: &str) -> bool {
    error_msg.contains("out of memory")
        || error_msg.contains("InitializeEncoder failed")
        || error_msg.contains("Cannot init NVENC")
        || error_msg.contains("OpenEncodeSessionEx failed")
        || error_msg.contains("No capable devices found")
        || error_msg.contains("Nothing was written into output file")
}

/// Check if an ffmpeg error is a subtitle mapping/copy issue.
/// These can be retried by dropping subtitle streams.
pub fn is_subtitle_error(error_msg: &str) -> bool {
    let lower = error_msg.to_lowercase();
    lower.contains("subtitle encoding currently only possible from text to text or bitmap to bitmap")
        || lower.contains("subtitle codec not supported")
        // "Subtitle codec 94213 is not supported." — MKV muxer rejecting mov_text/tx3g
        || lower.contains("subtitle codec") && lower.contains("is not supported")
        || lower.contains("error while opening encoder for output stream")
            && lower.contains("subtitle")
        || lower.contains("could not find tag for codec")
            && (lower.contains("subtitle") || lower.contains("hdmv_pgs"))
        || lower.contains("unknown encoder") && lower.contains("subtitle")
        // MP4 mov_text/tx3g subtitles cannot be copied to MKV; ffmpeg reports this as a
        // generic container incompatibility rather than a subtitle-specific error.
        || lower.contains("codec not currently supported in container")
}

/// Check if an ffmpeg error is a disk space issue.
pub fn is_disk_space_error(error_msg: &str) -> bool {
    error_msg.contains("Disk quota exceeded")
        || error_msg.contains("No space left on device")
        || error_msg.contains("ENOSPC")
}

/// Audio codec names that ffmpeg uses in its "Could not find tag for codec X"
/// message. Used by `is_audio_copy_error` to distinguish audio mismatches from
/// subtitle ones (mov_text, hdmv_pgs_subtitle, dvd_subtitle, …) without
/// over-matching.
///
/// Common offenders:
/// - `truehd`, `dts`, `eac3`, `ac3` — Blu-ray / DVD lossy & lossless tracks
///   that don't fit in MP4 without a re-encode.
/// - `flac`, `opus`, `vorbis` — open codecs older MP4 builds reject.
/// - `pcm_dvd`, `pcm_bluray`, `pcm_s16le`, `pcm_s24le` — raw PCM variants
///   that matroska/MP4 won't take via `-c:a copy`.
/// - `mp3`, `aac`, `alac` — included for completeness; rare but seen with
///   exotic container choices.
const AUDIO_COPY_CODECS: &[&str] = &[
    "truehd",
    "dts",
    "eac3",
    "ac3",
    "flac",
    "opus",
    "vorbis",
    "pcm_dvd",
    "pcm_bluray",
    "pcm_s16le",
    "pcm_s24le",
    "pcm_s16be",
    "pcm_s24be",
    "pcm_f32le",
    "mp3",
    "aac",
    "alac",
];

/// Check if an ffmpeg error indicates `-c:a copy` produced a stream the chosen
/// container can't accept. The classic case is a DVD's pcm_dvd audio being
/// stream-copied into MKV: the matroska muxer fails the header write, then
/// every other stream stalls and ffmpeg ends with the generic
/// "Nothing was written into output file" cascade. Recoverable by re-encoding.
///
/// "Could not find tag for codec X" is the most common form for MP4 muxers
/// (TrueHD/DTS/E-AC-3 from Blu-ray, FLAC in older ffmpeg). That message is
/// also emitted for subtitle codecs, so we only treat it as an audio-copy
/// error when X appears in `AUDIO_COPY_CODECS` — subtitle codecs (mov_text,
/// hdmv_pgs_subtitle, dvd_subtitle) are deliberately excluded and stay with
/// `is_subtitle_error` / the skip-subs retry.
pub fn is_audio_copy_error(error_msg: &str) -> bool {
    let lower = error_msg.to_lowercase();

    // matroska's wav-tag rejection — only emitted for PCM-family audio codecs
    // (pcm_dvd, pcm_bluray) being stream-copied into MKV. Unambiguous.
    if lower.contains("no wav codec tag found") {
        return true;
    }

    // Header-write failure with "incorrect codec parameters". This *can* be
    // triggered by other stream types, but if a subtitle codec is the cause
    // is_subtitle_error catches it first via the muxer's specific message;
    // by the time we land here, audio re-encode is the right next step.
    if lower.contains("could not write header") && lower.contains("incorrect codec parameters") {
        return true;
    }

    // "Could not find tag for codec X ..." — match only when X is a known
    // audio codec. Subtitle codecs (mov_text, hdmv_pgs_subtitle) deliberately
    // do not appear in AUDIO_COPY_CODECS so they don't trigger here.
    if let Some(rest) = lower.split("could not find tag for codec ").nth(1) {
        // Codec name is the next whitespace-delimited token.
        let codec = rest
            .split(|c: char| c.is_whitespace() || c == ',')
            .next()
            .unwrap_or("");
        if AUDIO_COPY_CODECS.contains(&codec) {
            return true;
        }
    }

    false
}

/// Distill multi-line ffmpeg stderr into the most informative error context.
///
/// ffmpeg often prints the *root cause* early (e.g. "No wav codec tag found
/// for codec pcm_dvd") then cascades into generic noise:
/// "Conversion failed!", "Nothing was written into output file...", a string
/// of "Error sending frames" warnings, etc. Showing only the tail buries the
/// real cause, so this scans the full stream, surfaces the substantive
/// error/warning lines, and de-duplicates the cascade.
fn summarize_ffmpeg_error(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(|l| l.trim_end())
        .filter(|l| !l.is_empty())
        .collect();

    // ffmpeg progress lines and trivially generic cascade summaries — never useful.
    let is_noise = |l: &str| {
        let t = l.trim_start();
        t.starts_with("frame=")
            || t.starts_with("size=")
            || t.starts_with("Last message repeated")
            || t == "Conversion failed!"
    };

    // Lines that look like an actual ffmpeg error/warning worth surfacing.
    let is_error = |l: &str| {
        let lower = l.to_lowercase();
        lower.contains("error")
            || lower.contains("invalid")
            || lower.contains("could not")
            || lower.contains("failed")
            || lower.contains("cannot")
            || lower.contains("not supported")
            || lower.contains("unsupported")
            || lower.contains("no wav codec")
            || lower.contains("no such")
            || lower.contains("incorrect codec")
            || lower.contains("nothing was written")
    };

    // Strip the "[module @ 0xADDR]" prefix that varies per run, so identical
    // messages cascading through different modules dedupe cleanly.
    fn body(line: &str) -> &str {
        if let Some(rest) = line.strip_prefix('[') {
            if let Some(end) = rest.find("] ") {
                return rest[end + 2..].trim_start();
            }
        }
        line
    }

    let mut seen = std::collections::HashSet::new();
    let mut picked: Vec<&str> = Vec::new();
    for line in &lines {
        if is_noise(line) || !is_error(line) {
            continue;
        }
        if seen.insert(body(line).to_string()) {
            picked.push(line);
            if picked.len() >= 4 {
                break;
            }
        }
    }

    if !picked.is_empty() {
        return picked.join(" | ");
    }

    // Fallback: last 3 non-noise lines. Better than nothing.
    let tail: Vec<&str> = lines.iter().copied().filter(|l| !is_noise(l)).collect();
    let start = tail.len().saturating_sub(3);
    let result = tail[start..].join(" | ");
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
        assert!(is_session_limit_error("Cannot init NVENC encoder"));
        assert!(is_session_limit_error("OpenEncodeSessionEx failed"));
        assert!(is_session_limit_error(
            "InitializeEncoder failed: out of memory"
        ));
        assert!(!is_session_limit_error("some other error"));
        // "Nothing was written" is the common NVENC session exhaustion pattern
        assert!(is_session_limit_error(
            "Nothing was written into output file"
        ));
        // Generic exit codes should NOT be classified as session limit
        assert!(!is_session_limit_error(
            "ffmpeg exited with status exit status: 69"
        ));
    }

    #[test]
    fn test_is_subtitle_error() {
        assert!(is_subtitle_error(
            "Subtitle encoding currently only possible from text to text or bitmap to bitmap"
        ));
        assert!(is_subtitle_error(
            "Could not find tag for codec hdmv_pgs_subtitle"
        ));
        // mov_text / tx3g subtitles in MP4 → MKV
        assert!(is_subtitle_error(
            "Could not find tag for codec mov_text in stream #0:2, codec not currently supported in container"
        ));
        assert!(!is_subtitle_error("some other error"));
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
    fn test_is_audio_copy_error() {
        // The exact pcm_dvd-into-MKV failure that prompted this code path
        assert!(is_audio_copy_error(
            "[matroska @ 0x123] No wav codec tag found for codec pcm_dvd"
        ));
        // pcm_bluray sister case: matroska rejects raw PCM the same way
        assert!(is_audio_copy_error(
            "[matroska @ 0x123] No wav codec tag found for codec pcm_bluray"
        ));
        assert!(is_audio_copy_error(
            "[out#0/matroska @ 0x123] Could not write header (incorrect codec parameters ?): Invalid argument"
        ));

        // MP4 muxer rejecting Blu-ray / DVD audio that won't fit in MOV/MP4.
        // These are the cases the broadened predicate is meant to catch.
        assert!(is_audio_copy_error(
            "[mp4 @ 0x123] Could not find tag for codec truehd in stream #0:1, codec not currently supported in container"
        ));
        assert!(is_audio_copy_error(
            "[mp4 @ 0x123] Could not find tag for codec dts in stream #0:1, codec not currently supported in container"
        ));
        assert!(is_audio_copy_error(
            "[mp4 @ 0x123] Could not find tag for codec eac3 in stream #0:1"
        ));
        assert!(is_audio_copy_error(
            "[mp4 @ 0x123] Could not find tag for codec ac3 in stream #0:1"
        ));
        assert!(is_audio_copy_error(
            "[mp4 @ 0x123] Could not find tag for codec flac in stream #0:1"
        ));
        assert!(is_audio_copy_error(
            "[mp4 @ 0x123] Could not find tag for codec opus in stream #0:1"
        ));

        // PGS / mov_text "Could not find tag for codec X" is subtitle-side; the
        // retry path for those is `force_skip_subs`, not audio re-encode. These
        // MUST stay false even after broadening — that's the whole point of
        // the AUDIO_COPY_CODECS allow-list.
        assert!(!is_audio_copy_error(
            "Could not find tag for codec hdmv_pgs_subtitle in stream #0:2"
        ));
        assert!(!is_audio_copy_error(
            "Could not find tag for codec mov_text in stream #0:2, codec not currently supported in container"
        ));
        assert!(!is_audio_copy_error(
            "Could not find tag for codec dvd_subtitle in stream #0:2"
        ));
        assert!(!is_audio_copy_error(
            "Could not find tag for codec dvb_subtitle in stream #0:2"
        ));

        // Generic / unrelated noise should never match.
        assert!(!is_audio_copy_error("some unrelated ffmpeg error"));
        assert!(!is_audio_copy_error(
            "Could not find tag for codec h264 in stream #0:0"
        ));
    }

    #[test]
    fn test_summarize_extracts_root_cause_not_cascade() {
        // Real stderr from a pcm_dvd → MKV failure. The first two lines are the root
        // cause; everything below them is cascade noise. The summary must surface
        // the cause, not just the trailing "Nothing was written" muxer complaint.
        let stderr = "\
Input #0, mpeg, from 'pipe:0':
  Duration: N/A, start: 0.287267, bitrate: N/A
  Stream #0:2[0xa0]: Audio: pcm_dvd, 48000 Hz, stereo, s16, 1536 kb/s
Stream mapping:
  Stream #0:1 -> #0:0 (mpeg2video (native) -> hevc (hevc_nvenc))
  Stream #0:2 -> #0:1 (copy)
[matroska @ 0x5ed165ece880] No wav codec tag found for codec pcm_dvd
[out#0/matroska @ 0x5ed165ece780] Could not write header (incorrect codec parameters ?): Invalid argument
[vf#0:0 @ 0x5ed165d3e0c0] Error sending frames to consumers: Invalid argument
[vf#0:0 @ 0x5ed165d3e0c0] Task finished with error code: -22 (Invalid argument)
[vf#0:0 @ 0x5ed165d3e0c0] Terminating thread with return code -22 (Invalid argument)
[mpeg @ 0x5ed165d2e940] Packet corrupt (stream = 1, dts = NOPTS).
[out#0/matroska @ 0x5ed165ece780] Nothing was written into output file, because at least one of its streams received no packets.
frame=    0 fps=0.0 q=0.0 Lsize=       0KiB time=N/A bitrate=N/A speed=N/A
Conversion failed!
";
        let summary = summarize_ffmpeg_error(stderr);
        assert!(
            summary.contains("No wav codec tag found for codec pcm_dvd"),
            "Summary must surface the root cause; got: {}",
            summary
        );
        assert!(
            !summary.contains("Conversion failed!"),
            "Summary must drop the trivial trailing 'Conversion failed!'; got: {}",
            summary
        );
        assert!(
            !summary.contains("frame="),
            "Summary must drop progress lines; got: {}",
            summary
        );
    }

    #[test]
    fn test_summarize_dedupes_cascade_with_different_module_prefixes() {
        // Same message body repeated under different "[module @ 0xADDR]" prefixes
        // should collapse into a single entry.
        let stderr = "\
[vf#0:0 @ 0x111] Error sending frames to consumers: Invalid argument
[vf#0:1 @ 0x222] Error sending frames to consumers: Invalid argument
[vf#0:2 @ 0x333] Error sending frames to consumers: Invalid argument
";
        let summary = summarize_ffmpeg_error(stderr);
        assert_eq!(
            summary.matches("Error sending frames to consumers").count(),
            1,
            "Repeated body should appear once; got: {}",
            summary
        );
    }

    #[test]
    fn test_summarize_falls_back_to_tail_when_no_keywords() {
        let stderr = "step one\nstep two\nstep three\nstep four\n";
        let summary = summarize_ffmpeg_error(stderr);
        assert_eq!(summary, "step two | step three | step four");
    }

    #[test]
    fn test_summarize_handles_empty() {
        assert_eq!(summarize_ffmpeg_error(""), "unknown error");
        assert_eq!(summarize_ffmpeg_error("\n\n   \n"), "unknown error");
    }

    /// Helper: write `bytes` zero bytes to `path`. We need an actual file since
    /// validate_output stats it; an ffprobe-readable output isn't required for
    /// the size-floor checks we want to exercise (those bail before ffprobe).
    fn write_zeros(path: &Path, bytes: u64) {
        use std::io::Write;
        let mut f = std::fs::File::create(path).unwrap();
        let chunk = vec![0u8; 64 * 1024];
        let mut remaining = bytes;
        while remaining > 0 {
            let n = remaining.min(chunk.len() as u64) as usize;
            f.write_all(&chunk[..n]).unwrap();
            remaining -= n as u64;
        }
    }

    #[test]
    fn test_validate_output_rejects_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.mkv");
        let output = dir.path().join("output.mkv");
        write_zeros(&source, 100 * 1024 * 1024); // 100 MB source
        write_zeros(&output, 0); // empty output
        let err = validate_output(&output, &source, 60.0).unwrap_err();
        assert!(
            err.to_string().contains("empty"),
            "expected empty-file error, got: {}",
            err
        );
    }

    #[test]
    fn test_validate_output_rejects_tiny_nonempty_output() {
        // A 1KB output for a 60s source: well below the 50 kbps × 60s = 375KB floor.
        // The previous "1% of source size" check would have *passed* this for a small
        // source; the new check correctly rejects it as truncation/corruption.
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.mkv");
        let output = dir.path().join("output.mkv");
        write_zeros(&source, 50 * 1024); // 50KB source — old check would allow tiny outputs
        write_zeros(&output, 1024); // 1KB output
        let err = validate_output(&output, &source, 60.0).unwrap_err();
        assert!(
            err.to_string().contains("suspiciously small"),
            "expected size-floor error, got: {}",
            err
        );
    }

    #[test]
    fn test_validate_output_passes_size_floor_for_60s_at_50kbps() {
        // For a 60s source, 50 kbps × 60s / 8 = 375,000 bytes. An output at or above
        // that size should clear the size-floor check (it'll fail later on ffprobe
        // since we wrote zeros, but that's a separate later step). Using 400KB to
        // be comfortably above the floor.
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.mkv");
        let output = dir.path().join("output.mkv");
        write_zeros(&source, 100 * 1024 * 1024); // arbitrary large source
        write_zeros(&output, 400 * 1024); // 400KB > 375KB floor

        // We expect validate_output to fail on a *later* check (ffprobe) but
        // crucially NOT on the size floor. Distinguish by error message.
        let err = validate_output(&output, &source, 60.0).unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains("suspiciously small"),
            "size-floor check should pass for 400KB / 60s; got: {}",
            msg
        );
    }

    #[test]
    fn test_validate_output_passes_heavy_compression_ratio() {
        // The motivating case: a heavily-compressed output that's <1% of source
        // size used to false-trip the old `out_size < src_size / 100` check.
        // With the new bitrate-floor check that ratio is irrelevant; only the
        // bytes-per-second of the output matters.
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.mkv");
        let output = dir.path().join("output.mkv");
        // 100 MB "source" stand-in, 800 KB output (0.8% ratio — old check rejects).
        // 800 KB over 60s ≈ 109 kbps, comfortably above the 50 kbps floor.
        write_zeros(&source, 100 * 1024 * 1024);
        write_zeros(&output, 800 * 1024);
        let err = validate_output(&output, &source, 60.0).unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains("suspiciously small"),
            "heavy compression should not trip the size floor; got: {}",
            msg
        );
    }
}
