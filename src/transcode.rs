use anyhow::{bail, Context, Result};
use std::io::BufRead;
use std::os::unix::fs::{chown, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::TargetConfig;
use crate::gpu::{GpuInfo, GpuKind};
use crate::probe;
use crate::util::format_size;

// ── Completion-marker side-car ──────────────────────────────────────────────
//
// A successful transcode writes `<output>.hvac.complete` alongside the output
// file. The marker is the only trustworthy signal that the previous run
// finished writing — ffprobe-only validation can pass on a half-written file
// with a plausible header and duration, which is how the adopt path silently
// loses data when a killed run left a partial `.transcoded.*` behind.

/// Filename suffix for the sidecar completion marker.
pub const MARKER_SUFFIX: &str = ".hvac.complete";

/// Contents of the sidecar marker file. Small JSON blob via serde_json.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct CompletionMarker {
    /// Size (in bytes) of the source file at the moment the encode completed.
    /// On resume we re-read the source and refuse to adopt if it differs.
    pub source_size: u64,
    /// Duration (seconds) of the source as measured before encoding.
    pub duration_secs: f64,
    /// ISO-8601 (UTC) timestamp when the marker was written.
    pub completed_at: String,
}

/// Path of the sidecar completion marker for a given output file.
pub fn marker_path(output: &Path) -> PathBuf {
    let mut s = output.as_os_str().to_owned();
    s.push(MARKER_SUFFIX);
    PathBuf::from(s)
}

/// Write the completion marker next to `output`. Best-effort: failure to
/// write is logged but does not fail the encode (the encode itself is fine;
/// a missing marker just means the next run will re-encode, never that
/// data is silently corrupted).
pub fn write_marker(output: &Path, source_size: u64, duration_secs: f64) -> Result<()> {
    let marker = CompletionMarker {
        source_size,
        duration_secs,
        completed_at: iso8601_now(),
    };
    let path = marker_path(output);
    let json = serde_json::to_string(&marker).context("serialize completion marker")?;
    std::fs::write(&path, json).with_context(|| format!("write completion marker {:?}", path))?;
    Ok(())
}

/// Read and parse a sidecar marker for `output`. Returns None if the marker
/// is absent or unparseable.
pub fn read_marker(output: &Path) -> Option<CompletionMarker> {
    let path = marker_path(output);
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice::<CompletionMarker>(&bytes).ok()
}

/// True when `output` exists AND has a sidecar marker AND the marker's
/// `source_size` matches the current size of `source`. Existence of the
/// output file is checked first — without it, a stale marker (sidecar that
/// outlived the file it described) would falsely claim safety.
pub fn marker_valid_for_source(output: &Path, source: &Path) -> bool {
    if !output.exists() {
        return false;
    }
    let Some(marker) = read_marker(output) else {
        return false;
    };
    let Ok(meta) = std::fs::metadata(source) else {
        return false;
    };
    marker.source_size == meta.len()
}

/// Best-effort delete of any sidecar marker next to `output`.
pub fn remove_marker(output: &Path) {
    let _ = std::fs::remove_file(marker_path(output));
}

fn iso8601_now() -> String {
    // Minimal RFC3339 / ISO-8601 UTC formatter: "YYYY-MM-DDTHH:MM:SSZ".
    // Avoids pulling in chrono just for a timestamp string.
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = unix_to_ymdhms(secs);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, mi, s)
}

/// Convert a unix-epoch second count to (year, month, day, hour, min, sec) in UTC.
/// Civil-from-days algorithm by Howard Hinnant — exact for the full unix range.
fn unix_to_ymdhms(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400) as u32;
    let h = tod / 3600;
    let mi = (tod % 3600) / 60;
    let s = tod % 60;

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if mo <= 2 { y + 1 } else { y };
    (year as i32, mo, d, h, mi, s)
}

/// Color / HDR metadata to forward from source to output. All fields optional;
/// only present ones are emitted as ffmpeg flags. Without these, HDR10/HLG
/// sources transcode with the wrong color tags and play back washed out.
#[derive(Debug, Clone, Default)]
pub struct ColorMetadata {
    pub color_primaries: Option<String>,
    pub color_transfer: Option<String>,
    pub color_space: Option<String>,
    pub color_range: Option<String>,
    pub master_display: Option<String>,
    pub max_cll: Option<String>,
}

impl ColorMetadata {
    /// Pull color fields off a probed MediaInfo.
    pub fn from_media_info(info: &probe::MediaInfo) -> Self {
        ColorMetadata {
            color_primaries: info.color_primaries.clone(),
            color_transfer: info.color_transfer.clone(),
            color_space: info.color_space.clone(),
            color_range: info.color_range.clone(),
            master_display: info.master_display.clone(),
            max_cll: info.max_cll.clone(),
        }
    }
}

/// Build the list of color/HDR ffmpeg args to forward.
/// `-color_primaries`, `-color_trc`, `-colorspace`, `-color_range` work with
/// every encoder we use (NVENC, VAAPI, VideoToolbox); `-master_display` and
/// `-max_cll` are NVENC-specific HDR10 passthrough flags so we emit them
/// only when targeting an Nvidia GPU. Pure function — easy to unit-test.
fn build_color_args(color: &ColorMetadata, gpu_kind: &GpuKind) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(v) = color.color_primaries.as_deref() {
        args.push("-color_primaries".to_string());
        args.push(v.to_string());
    }
    if let Some(v) = color.color_transfer.as_deref() {
        args.push("-color_trc".to_string());
        args.push(v.to_string());
    }
    if let Some(v) = color.color_space.as_deref() {
        args.push("-colorspace".to_string());
        args.push(v.to_string());
    }
    if let Some(v) = color.color_range.as_deref() {
        args.push("-color_range".to_string());
        args.push(v.to_string());
    }
    if matches!(gpu_kind, GpuKind::Nvidia) {
        if let Some(v) = color.master_display.as_deref() {
            args.push("-master_display".to_string());
            args.push(v.to_string());
        }
        if let Some(v) = color.max_cll.as_deref() {
            args.push("-max_cll".to_string());
            args.push(v.to_string());
        }
    }
    args
}

/// Append color/HDR ffmpeg args to `cmd` based on what's present in `color`.
fn append_color_args(cmd: &mut Command, color: &ColorMetadata, gpu_kind: &GpuKind) {
    for a in build_color_args(color, gpu_kind) {
        cmd.arg(a);
    }
}

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
/// If `subtitle_codec_override` is `Some(codec)`, that codec replaces the configured
/// `subtitle_codec` — used to retry after a copy-incompatible subtitle codec fails,
/// before falling back to dropping subs entirely.
#[allow(clippy::too_many_arguments)]
pub fn transcode(
    source: &Path,
    output: Option<&Path>,
    target: &TargetConfig,
    gpu: &GpuInfo,
    source_bitrate_kbps: u32,
    source_duration_secs: f64,
    source_pix_fmt: &str,
    color: &ColorMetadata,
    progress: Option<&AtomicU64>,
    speed: Option<&AtomicU64>,
    skip_subs: bool,
    force_reencode_audio: bool,
    subtitle_codec_override: Option<&str>,
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

    // Forward color/HDR metadata so HDR10/HLG sources don't lose their tags.
    append_color_args(&mut cmd, color, &gpu.kind);

    // Audio
    let audio_codec = if force_reencode_audio && target.audio_codec == "copy" {
        AUDIO_REENCODE_FALLBACK
    } else {
        target.audio_codec.as_str()
    };
    cmd.args(["-c:a", audio_codec]);

    // Subtitles
    apply_subtitle_args(&mut cmd, target, skip_subs, subtitle_codec_override);

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
        "Running{}{}{}: {:?}",
        if skip_subs { " (no subs)" } else { "" },
        if force_reencode_audio {
            " (audio re-encode)"
        } else {
            ""
        },
        if let Some(c) = subtitle_codec_override {
            format!(" (sub re-encode {})", c)
        } else {
            String::new()
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
        // Write the completion marker only AFTER the rename succeeds so a
        // failed rename doesn't orphan a markerless `.hvac_tmp_*` file.
        let final_size = std::fs::metadata(source).map(|m| m.len()).unwrap_or(0);
        if let Err(e) = write_marker(source, final_size, source_duration_secs) {
            log::warn!("Failed to write completion marker for {:?}: {}", source, e);
        }
        return Ok(source.to_path_buf());
    }

    // --no-overwrite path: marker sits next to the new output file.
    if let Err(e) = write_marker(&final_output, source_size, source_duration_secs) {
        log::warn!(
            "Failed to write completion marker for {:?}: {}",
            final_output,
            e
        );
    }
    Ok(final_output)
}

/// Transcode file(s) from inside an ISO by streaming them to ffmpeg via stdin.
/// Multiple inner paths are concatenated sequentially (e.g. Blu-ray chapters).
/// The ISO contents are piped directly to ffmpeg without extracting to disk.
/// `force_reencode_audio` overrides `audio_codec: copy` with `aac` for retries
/// after a copy-incompatible codec is detected.
/// `subtitle_codec_override`, when `Some(codec)`, replaces the configured
/// subtitle codec — used as a retry tier between `copy` and dropping subs.
/// `primary_audio_index`, when `Some(N)`, selects exactly one audio stream
/// (`-map 0:a:N`) instead of mapping every track. Used for disc images,
/// where leaving `-map 0:a?` can silently produce a commentary-only output
/// when ffmpeg's stream detection only catches one audio PID off the pipe.
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
    color: &ColorMetadata,
    progress: Option<&AtomicU64>,
    speed: Option<&AtomicU64>,
    skip_subs: bool,
    force_reencode_audio: bool,
    subtitle_codec_override: Option<&str>,
    primary_audio_index: Option<u32>,
) -> Result<PathBuf> {
    let final_output = output.to_path_buf();

    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-y"]);

    // Piped MPEG-TS / m2ts hides audio PIDs from the default 5MB / 5s probe
    // window; bump both so ffmpeg sees every audio stream and our
    // `-map 0:a:N` selector points at the track we actually intended.
    cmd.args([
        "-probesize",
        probe::PIPE_PROBESIZE,
        "-analyzeduration",
        probe::PIPE_ANALYZEDURATION,
    ]);

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

    // Forward color/HDR metadata (same as transcode())
    append_color_args(&mut cmd, color, &gpu.kind);

    // Audio
    let audio_codec = if force_reencode_audio && target.audio_codec == "copy" {
        AUDIO_REENCODE_FALLBACK
    } else {
        target.audio_codec.as_str()
    };
    cmd.args(["-c:a", audio_codec]);

    // Subtitles
    apply_subtitle_args(&mut cmd, target, skip_subs, subtitle_codec_override);

    // Map streams. The audio map is explicit for disc-image inputs (see
    // `apply_iso_audio_map`).
    cmd.args(["-map", "0:v:0"]);
    apply_iso_audio_map(&mut cmd, primary_audio_index);
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
        "Running (piped from ISO, audio=0:a:{}{}{}{}): {:?}",
        primary_audio_index
            .map(|n| n.to_string())
            .unwrap_or_else(|| "0?".to_string()),
        if skip_subs { ", no subs" } else { "" },
        if force_reencode_audio {
            ", audio re-encode"
        } else {
            ""
        },
        if let Some(c) = subtitle_codec_override {
            format!(", sub re-encode {}", c)
        } else {
            String::new()
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

    // For ISO encodes the marker's `source_size` is the size of the .iso/.img
    // file itself. `marker_valid_for_source` compares it against the current
    // size of whatever path partition.rs hands in as the "source", and for
    // disc images that's the disc image path — *not* the inner stream we
    // actually piped to ffmpeg. Storing the ISO's size keeps that comparison
    // consistent so a future resume adopts the output iff the disc is byte-
    // identical to the one we encoded from.
    let iso_size = std::fs::metadata(iso_path).map(|m| m.len()).unwrap_or(0);
    if let Err(e) = write_marker(&final_output, iso_size, source_duration_secs) {
        log::warn!(
            "Failed to write completion marker for {:?}: {}",
            final_output,
            e
        );
    }

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

/// Audio codec used when `audio_codec: copy` produces a stream the chosen
/// container can't accept (e.g. pcm_dvd in MKV). AAC is universally muxable.
const AUDIO_REENCODE_FALLBACK: &str = "aac";

/// Pick a container-appropriate text subtitle codec to try before falling
/// back to dropping subs entirely. These succeed only when source subs are
/// convertible (text-based: srt, ass, mov_text, webvtt). Bitmap subs
/// (PGS, dvdsub, dvbsub) still fail this tier and force the skip-subs retry.
pub fn subtitle_reencode_fallback(container: &str) -> &'static str {
    match container {
        "mp4" | "m4v" | "mov" => "mov_text",
        // mkv, webm, and anything else: srt is the most broadly compatible text codec.
        _ => "srt",
    }
}

/// Append the audio `-map` argument for an ISO transcode.
///
/// Disc-image inputs are read from a stdin pipe; ffmpeg's default
/// probesize / analyzeduration miss audio PIDs on Blu-ray m2ts, so an
/// implicit `-map 0:a?` can silently end up with the commentary as the
/// sole output track. We always pick exactly one audio stream:
///
///   - `Some(n)` from `probe::pick_primary_audio` → `-map 0:a:N`
///   - `None` (caller couldn't decide) → `-map 0:a:0?` (first audio,
///     `?` so silent-video discs still encode rather than erroring on
///     a missing stream).
fn apply_iso_audio_map(cmd: &mut Command, primary_audio_index: Option<u32>) {
    match primary_audio_index {
        Some(n) => {
            let map = format!("0:a:{}", n);
            cmd.args(["-map", &map]);
        }
        None => {
            cmd.args(["-map", "0:a:0?"]);
        }
    }
}

/// Append the appropriate `-c:s` argument(s) to the ffmpeg command.
/// - When `skip_subs` is true: caller separately omits the subtitle map; nothing here.
/// - When an override is provided: use that codec (subtitle re-encode retry tier).
/// - Otherwise: honor the configured `target.subtitle_codec` if it's `copy`.
///   (Other configured codecs are passed through unchanged for forward compatibility.)
fn apply_subtitle_args(
    cmd: &mut Command,
    target: &TargetConfig,
    skip_subs: bool,
    subtitle_codec_override: Option<&str>,
) {
    if skip_subs {
        return;
    }
    if let Some(codec) = subtitle_codec_override {
        cmd.args(["-c:s", codec]);
        return;
    }
    if target.subtitle_codec == "copy" {
        cmd.args(["-c:s", "copy"]);
    } else if !target.subtitle_codec.is_empty() {
        cmd.args(["-c:s", target.subtitle_codec.as_str()]);
    }
}

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

/// Check if an ffmpeg error indicates `-c:a copy` produced a stream the chosen
/// container can't accept. The classic case is a DVD's pcm_dvd audio being
/// stream-copied into MKV: the matroska muxer fails the header write, then
/// every other stream stalls and ffmpeg ends with the generic
/// "Nothing was written into output file" cascade. Recoverable by re-encoding.
///
/// We deliberately match only patterns that are unambiguously audio. The
/// generic "Could not find tag for codec X" message can be either audio
/// or subtitle (mov_text, hdmv_pgs_subtitle) — that one is left to
/// `is_subtitle_error` and the skip-subs retry.
pub fn is_audio_copy_error(error_msg: &str) -> bool {
    let lower = error_msg.to_lowercase();
    // matroska's wav-tag rejection — only emitted for PCM-family audio codecs
    // (pcm_dvd, pcm_bluray) being stream-copied into MKV. Unambiguous.
    lower.contains("no wav codec tag found")
        // Header-write failure with "incorrect codec parameters". This *can* be
        // triggered by other stream types, but if a subtitle codec is the cause
        // is_subtitle_error catches it first via the muxer's specific message;
        // by the time we land here, audio re-encode is the right next step.
        || (lower.contains("could not write header")
            && lower.contains("incorrect codec parameters"))
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

    // Delete the transcoded sibling's marker AFTER the rename succeeds. A
    // failed rename leaves the marker intact alongside the still-valid file
    // so the next run can adopt it; a pre-rename delete would orphan it.
    std::fs::rename(transcoded, original)
        .with_context(|| format!("Failed to replace {:?} with transcoded version", original))?;
    remove_marker(transcoded);

    let final_size = std::fs::metadata(original).map(|m| m.len()).unwrap_or(0);
    if let Err(e) = write_marker(original, final_size, source_duration_secs) {
        log::warn!(
            "Failed to write completion marker for {:?}: {}",
            original,
            e
        );
    }

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
        assert!(is_audio_copy_error(
            "[out#0/matroska @ 0x123] Could not write header (incorrect codec parameters ?): Invalid argument"
        ));
        // PGS / mov_text "Could not find tag for codec X" is subtitle-side and ambiguous;
        // we deliberately leave it to is_subtitle_error / skip-subs retry.
        assert!(!is_audio_copy_error(
            "Could not find tag for codec hdmv_pgs_subtitle"
        ));
        assert!(!is_audio_copy_error(
            "Could not find tag for codec mov_text in stream #0:2, codec not currently supported in container"
        ));
        assert!(!is_audio_copy_error("some unrelated ffmpeg error"));
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

    fn target_with(container: &str, subtitle_codec: &str) -> TargetConfig {
        TargetConfig {
            codec: "hevc".to_string(),
            quality: 28,
            preset: "slow".to_string(),
            max_width: 3840,
            max_height: 2160,
            max_bitrate_kbps: 0,
            container: container.to_string(),
            audio_codec: "copy".to_string(),
            subtitle_codec: subtitle_codec.to_string(),
        }
    }

    /// Collect ffmpeg args as Strings, since std::process::Command doesn't
    /// expose them as &str directly in a stable form.
    fn cmd_args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn test_subtitle_reencode_fallback_picks_per_container() {
        assert_eq!(subtitle_reencode_fallback("mkv"), "srt");
        assert_eq!(subtitle_reencode_fallback("webm"), "srt");
        assert_eq!(subtitle_reencode_fallback("mp4"), "mov_text");
        assert_eq!(subtitle_reencode_fallback("m4v"), "mov_text");
        assert_eq!(subtitle_reencode_fallback("mov"), "mov_text");
        // Unknown containers default to srt (broadest text-codec compatibility).
        assert_eq!(subtitle_reencode_fallback("avi"), "srt");
    }

    #[test]
    fn test_apply_subtitle_args_default_copy() {
        let target = target_with("mkv", "copy");
        let mut cmd = Command::new("ffmpeg");
        apply_subtitle_args(&mut cmd, &target, false, None);
        let args = cmd_args(&cmd);
        // Configured `copy` produces `-c:s copy` adjacency.
        let pos = args.iter().position(|a| a == "-c:s").expect("has -c:s");
        assert_eq!(args[pos + 1], "copy");
    }

    #[test]
    fn test_apply_subtitle_args_skip_subs_emits_nothing() {
        let target = target_with("mkv", "copy");
        let mut cmd = Command::new("ffmpeg");
        apply_subtitle_args(&mut cmd, &target, true, None);
        let args = cmd_args(&cmd);
        assert!(
            !args.iter().any(|a| a == "-c:s"),
            "skip_subs should emit no -c:s; got {:?}",
            args
        );
    }

    #[test]
    fn test_apply_subtitle_args_override_replaces_copy() {
        let target = target_with("mkv", "copy");
        let mut cmd = Command::new("ffmpeg");
        apply_subtitle_args(&mut cmd, &target, false, Some("srt"));
        let args = cmd_args(&cmd);
        let pos = args.iter().position(|a| a == "-c:s").expect("has -c:s");
        assert_eq!(args[pos + 1], "srt");
        // Should appear exactly once — not both `copy` and `srt`.
        assert_eq!(args.iter().filter(|a| *a == "-c:s").count(), 1);
    }

    #[test]
    fn test_apply_subtitle_args_override_for_mp4() {
        let target = target_with("mp4", "copy");
        let mut cmd = Command::new("ffmpeg");
        apply_subtitle_args(
            &mut cmd,
            &target,
            false,
            Some(subtitle_reencode_fallback(&target.container)),
        );
        let args = cmd_args(&cmd);
        let pos = args.iter().position(|a| a == "-c:s").expect("has -c:s");
        assert_eq!(args[pos + 1], "mov_text");
    }

    #[test]
    fn test_apply_subtitle_args_override_wins_over_skip_subs_false_path() {
        // skip_subs=true short-circuits before any override is applied — sanity check
        // that override does NOT leak through when the caller has decided to drop subs.
        let target = target_with("mkv", "copy");
        let mut cmd = Command::new("ffmpeg");
        apply_subtitle_args(&mut cmd, &target, true, Some("srt"));
        let args = cmd_args(&cmd);
        assert!(!args.iter().any(|a| a == "-c:s"));
    }

    // ── ISO audio mapping ────────────────────────────────────────────────────
    //
    // The whole point of the disc-image audio plumbing is that we never emit
    // `-map 0:a?` for piped Blu-ray / DVD input. These tests pin the wire
    // format so a future refactor can't quietly regress to mapping every PID.

    #[test]
    fn test_apply_iso_audio_map_with_primary_index() {
        let mut cmd = Command::new("ffmpeg");
        apply_iso_audio_map(&mut cmd, Some(2));
        let args = cmd_args(&cmd);
        let pos = args
            .iter()
            .position(|a| a == "-map")
            .expect("must emit a -map");
        assert_eq!(args[pos + 1], "0:a:2");
        // Exactly one -map (we never want to also emit 0:a? as a "fallback").
        assert_eq!(args.iter().filter(|a| *a == "-map").count(), 1);
    }

    #[test]
    fn test_apply_iso_audio_map_without_primary_falls_back_to_first() {
        // No probed audio stream → first audio with `?` so a silent-video
        // disc still encodes instead of erroring on the missing stream.
        let mut cmd = Command::new("ffmpeg");
        apply_iso_audio_map(&mut cmd, None);
        let args = cmd_args(&cmd);
        let pos = args
            .iter()
            .position(|a| a == "-map")
            .expect("must emit a -map");
        assert_eq!(args[pos + 1], "0:a:0?");
    }

    #[test]
    fn test_apply_iso_audio_map_never_emits_open_match() {
        // The regression we're guarding against: `-map 0:a?` would pull in
        // every audio PID, including commentary. Neither branch must emit it.
        let mut cmd_some = Command::new("ffmpeg");
        apply_iso_audio_map(&mut cmd_some, Some(0));
        let mut cmd_none = Command::new("ffmpeg");
        apply_iso_audio_map(&mut cmd_none, None);
        for cmd in [&cmd_some, &cmd_none] {
            let args = cmd_args(cmd);
            assert!(
                !args.iter().any(|a| a == "0:a?"),
                "apply_iso_audio_map must never emit `0:a?`: {:?}",
                args
            );
        }
    }

    // ── Color metadata arg-building tests ────────────────────────────────────
    //
    // build_color_args is the single source of truth for which ffmpeg flags get
    // emitted for a given source's color metadata. These tests pin the exact
    // wire format so a refactor can't accidentally drop tags or use the wrong
    // flag name (a common ffmpeg gotcha: `-color_trc` not `-color_transfer`).

    #[test]
    fn test_build_color_args_empty_when_no_metadata() {
        let color = ColorMetadata::default();
        let args = build_color_args(&color, &GpuKind::Nvidia);
        assert!(args.is_empty());
    }

    #[test]
    fn test_build_color_args_basic_tags_emitted() {
        let color = ColorMetadata {
            color_primaries: Some("bt2020".to_string()),
            color_transfer: Some("smpte2084".to_string()),
            color_space: Some("bt2020nc".to_string()),
            color_range: Some("tv".to_string()),
            ..ColorMetadata::default()
        };
        let args = build_color_args(&color, &GpuKind::Nvidia);
        assert_eq!(
            args,
            vec![
                "-color_primaries",
                "bt2020",
                "-color_trc",
                "smpte2084",
                "-colorspace",
                "bt2020nc",
                "-color_range",
                "tv",
            ]
        );
    }

    #[test]
    fn test_build_color_args_nvenc_emits_hdr10() {
        let color = ColorMetadata {
            color_primaries: Some("bt2020".to_string()),
            color_transfer: Some("smpte2084".to_string()),
            color_space: Some("bt2020nc".to_string()),
            master_display: Some(
                "G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(40000000,1)".to_string(),
            ),
            max_cll: Some("1000,400".to_string()),
            ..ColorMetadata::default()
        };
        let args = build_color_args(&color, &GpuKind::Nvidia);
        assert!(args.iter().any(|a| a == "-master_display"));
        assert!(args.iter().any(|a| a == "-max_cll"));
        assert!(args.iter().any(|a| a == "1000,400"));
    }

    #[test]
    fn test_build_color_args_vaapi_skips_hdr10_flags() {
        // -master_display and -max_cll are NVENC-specific. Other encoders
        // would error on these flags, so they must NOT be emitted.
        let color = ColorMetadata {
            color_primaries: Some("bt2020".to_string()),
            master_display: Some("G(0,0)B(0,0)R(0,0)WP(0,0)L(1,0)".to_string()),
            max_cll: Some("1000,400".to_string()),
            ..ColorMetadata::default()
        };
        let intel_args = build_color_args(&color, &GpuKind::Intel);
        assert!(intel_args.iter().any(|a| a == "-color_primaries"));
        assert!(!intel_args.iter().any(|a| a == "-master_display"));
        assert!(!intel_args.iter().any(|a| a == "-max_cll"));

        let apple_args = build_color_args(&color, &GpuKind::Apple);
        assert!(!apple_args.iter().any(|a| a == "-master_display"));
        assert!(!apple_args.iter().any(|a| a == "-max_cll"));
    }

    #[test]
    fn test_build_color_args_partial_metadata() {
        // Source with only primaries set (e.g. SDR BT.709) — emit just that.
        let color = ColorMetadata {
            color_primaries: Some("bt709".to_string()),
            ..ColorMetadata::default()
        };
        let args = build_color_args(&color, &GpuKind::Nvidia);
        assert_eq!(args, vec!["-color_primaries", "bt709"]);
    }

    #[test]
    fn test_color_metadata_from_media_info() {
        // Sanity: ColorMetadata::from_media_info copies the right fields.
        let info = probe::MediaInfo {
            codec: "hevc".to_string(),
            width: 3840,
            height: 2160,
            bitrate_kbps: 0,
            duration_secs: 0.0,
            pix_fmt: "yuv420p10le".to_string(),
            has_audio: false,
            has_subtitles: false,
            audio_streams: vec![],
            color_primaries: Some("bt2020".to_string()),
            color_transfer: Some("smpte2084".to_string()),
            color_space: Some("bt2020nc".to_string()),
            color_range: Some("tv".to_string()),
            master_display: Some("G(0,0)B(0,0)R(0,0)WP(0,0)L(1,0)".to_string()),
            max_cll: Some("1000,400".to_string()),
        };
        let cm = ColorMetadata::from_media_info(&info);
        assert_eq!(cm.color_primaries.as_deref(), Some("bt2020"));
        assert_eq!(cm.color_transfer.as_deref(), Some("smpte2084"));
        assert_eq!(cm.color_space.as_deref(), Some("bt2020nc"));
        assert_eq!(cm.color_range.as_deref(), Some("tv"));
        assert_eq!(cm.max_cll.as_deref(), Some("1000,400"));
    }
}
