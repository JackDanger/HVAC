use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::TargetConfig;

/// Default ffprobe watchdog timeout when no explicit value is supplied.
/// 30 seconds is generous for a healthy local disk but short enough that a
/// stale NFS / unresponsive SMB mount fails fast instead of hanging the run.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Poll interval for the watchdog loop. 50ms keeps wakeups cheap while
/// imposing < 100ms latency on top of the actual ffprobe runtime.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Grace window after `kill()` during which we still try to reap the child.
/// SIGKILL doesn't preempt processes stuck in uninterruptible IO (the exact
/// failure mode this watchdog exists to defeat — stale NFS / unresponsive
/// SMB), so a blocking `wait()` after `kill()` would re-introduce the hang
/// the watchdog just avoided. We poll for a short window and then bail —
/// the OS reaps the zombie when the syscall eventually completes.
const KILL_REAP_GRACE: Duration = Duration::from_millis(500);

/// Wait for `child` to exit, killing it and returning a clear error if the
/// watchdog fires first. `descriptor` is woven into the timeout error message
/// so the user can tell which probe got stuck.
fn wait_with_timeout(
    mut child: Child,
    timeout: Duration,
    descriptor: &str,
) -> Result<std::process::Output> {
    // Drain stdout and stderr in background threads. Without this, a large
    // JSON output (>64 KB pipe buffer) causes ffprobe to block on write, so
    // try_wait() never sees it exit and the watchdog fires spuriously.
    let stdout_thread = child.stdout.take().map(|mut r| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut r, &mut buf);
            buf
        })
    });
    let stderr_thread = child.stderr.take().map(|mut r| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut r, &mut buf);
            buf
        })
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait().context("Failed to poll ffprobe child")? {
            Some(status) => break status,
            None => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    // Try to reap the zombie, but only briefly — see
                    // KILL_REAP_GRACE for why we don't use blocking wait().
                    let reap_deadline = Instant::now() + KILL_REAP_GRACE;
                    while Instant::now() < reap_deadline {
                        if matches!(child.try_wait(), Ok(Some(_))) {
                            break;
                        }
                        std::thread::sleep(POLL_INTERVAL);
                    }
                    // Join the reader threads before bailing. kill() closes
                    // the pipes so they observe EOF and exit promptly. The
                    // ignored Result is fine — we're about to error out, so
                    // a panicked reader thread isn't actionable.
                    let _ = stdout_thread.map(|h| h.join());
                    let _ = stderr_thread.map(|h| h.join());
                    bail!(
                        "ffprobe timed out after {} reading {}; \
                         the source filesystem may be unresponsive",
                        format_timeout(timeout),
                        descriptor
                    );
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        }
    };

    let stdout = stdout_thread
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    let stderr = stderr_thread
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// Format a Duration for the user-facing timeout message without truncating
/// sub-second values. `as_secs()` alone would render 200ms as "0s".
fn format_timeout(d: Duration) -> String {
    if d.as_secs() >= 1 && d.subsec_millis() == 0 {
        format!("{}s", d.as_secs())
    } else if d.as_secs() == 0 {
        format!("{}ms", d.as_millis())
    } else {
        format!("{:.3}s", d.as_secs_f64())
    }
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct MediaInfo {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub bitrate_kbps: u32,
    pub duration_secs: f64,
    pub pix_fmt: String,
    pub has_audio: bool,
    pub has_subtitles: bool,
    /// HDR / color metadata. All optional — only emitted to ffmpeg when the
    /// source actually carries them. Without these flags, ffmpeg drops the
    /// tags and players show washed-out / wrong-gamma output for HDR sources.
    pub color_primaries: Option<String>,
    pub color_transfer: Option<String>,
    pub color_space: Option<String>,
    pub color_range: Option<String>,
    /// HDR10 mastering display, formatted for ffmpeg's `-master_display`:
    ///   `G(gx,gy)B(bx,by)R(rx,ry)WP(wpx,wpy)L(max,min)`
    /// Coordinates in 1/50000 units, luminance in 1/10000 nits.
    pub master_display: Option<String>,
    /// HDR10 max content + frame-average light level, formatted for
    /// ffmpeg's `-max_cll` as `max_cll,max_fall`.
    pub max_cll: Option<String>,
}

#[derive(Deserialize)]
struct FfprobeOutput {
    streams: Vec<FfprobeStream>,
    #[serde(default)]
    format: Option<FfprobeFormat>,
}

#[derive(Deserialize)]
struct FfprobeStream {
    codec_name: Option<String>,
    codec_type: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    pix_fmt: Option<String>,
    color_primaries: Option<String>,
    color_transfer: Option<String>,
    color_space: Option<String>,
    color_range: Option<String>,
    #[serde(default)]
    side_data_list: Option<Vec<FfprobeSideData>>,
    #[serde(default)]
    tags: Option<FfprobeTags>,
}

#[derive(Deserialize)]
struct FfprobeTags {
    #[serde(rename = "BPS")]
    bps: Option<String>,
}

#[derive(Deserialize)]
struct FfprobeFormat {
    bit_rate: Option<String>,
    duration: Option<String>,
}

/// One entry in ffprobe's `side_data_list`. Multiple side-data types share
/// the same JSON object shape (a `side_data_type` discriminant plus type-
/// specific fields), so we use a single permissive struct with all fields
/// optional and dispatch on `side_data_type`.
#[derive(Deserialize, Default)]
struct FfprobeSideData {
    side_data_type: Option<String>,

    // MasteringDisplayMetadata fields — rationals like "13250/50000".
    red_x: Option<String>,
    red_y: Option<String>,
    green_x: Option<String>,
    green_y: Option<String>,
    blue_x: Option<String>,
    blue_y: Option<String>,
    white_point_x: Option<String>,
    white_point_y: Option<String>,
    min_luminance: Option<String>,
    max_luminance: Option<String>,

    // ContentLightLevelMetadata fields — plain integers (nits).
    max_content: Option<u32>,
    max_average: Option<u32>,
}

/// Output of ffprobe's `-show_frames -read_intervals "%+#1"` — just the first
/// video frame, used to harvest HDR side-data attached to that frame.
#[derive(Deserialize)]
struct FfprobeFramesOutput {
    #[serde(default)]
    frames: Vec<FfprobeFrame>,
}

#[derive(Deserialize)]
struct FfprobeFrame {
    #[serde(default)]
    side_data_list: Option<Vec<FfprobeSideData>>,
}

/// Probe a file with the default timeout. Convenience wrapper for callers
/// that don't have a configured `Duration` (e.g. transcode-output validation).
pub fn probe_file(path: &Path) -> Result<MediaInfo> {
    probe_file_with_timeout(path, DEFAULT_PROBE_TIMEOUT)
}

/// Describe a process exit when it produced no stderr/stdout output.
/// Returns something like "exit code 1 (no output)" or "killed by signal 11 (SIGSEGV)".
fn format_exit_status(status: std::process::ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            let name = match sig {
                1 => " (SIGHUP)",
                2 => " (SIGINT)",
                3 => " (SIGQUIT)",
                4 => " (SIGILL)",
                6 => " (SIGABRT)",
                8 => " (SIGFPE)",
                9 => " (SIGKILL)",
                11 => " (SIGSEGV)",
                13 => " (SIGPIPE)",
                14 => " (SIGALRM)",
                15 => " (SIGTERM)",
                _ => "",
            };
            return format!("killed by signal {sig}{name} (no output)");
        }
    }
    if let Some(code) = status.code() {
        format!("exit code {code} (no output)")
    } else {
        "terminated abnormally (no output)".to_string()
    }
}

/// Probe a file, killing ffprobe and returning an error if it doesn't exit
/// within `timeout`. The watchdog protects against hangs caused by stale NFS
/// mounts or unresponsive network shares — without it, a single bad mount
/// blocks the entire scan forever.
pub fn probe_file_with_timeout(path: &Path, timeout: Duration) -> Result<MediaInfo> {
    let child = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_streams",
            "-show_format",
        ])
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn ffprobe")?;

    let descriptor = path.display().to_string();
    let output = wait_with_timeout(child, timeout, &descriptor)?;

    if !output.status.success() {
        // ffprobe writes progress with bare \r; str::lines() handles \r, \n, \r\n.
        // Take only the last non-empty line so \r-overwritten progress doesn't corrupt terminal.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let err_msg = stderr
            .lines()
            .rfind(|l| !l.trim().is_empty())
            .map(|s| s.to_string())
            .or_else(|| {
                // Some crashes produce output on stdout instead of stderr.
                let stdout = String::from_utf8_lossy(&output.stdout);
                stdout
                    .lines()
                    .rfind(|l| !l.trim().is_empty())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| format_exit_status(output.status));
        bail!("ffprobe failed for {:?}: {}", path, err_msg);
    }

    let mut info = parse_ffprobe_json(&output.stdout)?;

    // HDR10 mastering display + max-cll usually live in side-data attached
    // to frames, not the stream header. Some sources put them in the stream
    // header (parse_ffprobe_json picks those up); for the rest we probe the
    // first frame.
    //
    // Gate the second ffprobe on need: skip it when stream-header parsing
    // already gave us both fields, and when basic color tags say SDR (no
    // HDR-relevant transfer function). On a typical SDR-only library this
    // saves one subprocess per file across the whole scan.
    if needs_frame_side_data_probe(&info) {
        if let Ok(frame_out) = probe_first_frame_side_data(path, timeout) {
            apply_frame_side_data(&mut info, &frame_out);
        }
    }

    Ok(info)
}

/// True when stream-header parsing didn't already give us both HDR10
/// fields AND the file plausibly carries HDR (or we couldn't tell).
/// Avoids a wasted ffprobe on SDR sources.
fn needs_frame_side_data_probe(info: &MediaInfo) -> bool {
    if info.master_display.is_some() && info.max_cll.is_some() {
        return false;
    }
    // HDR transfer functions: smpte2084 = HDR10/PQ, arib-std-b67 = HLG.
    // bt2020 primaries also strongly suggest HDR. Anything else (bt709,
    // bt601, smpte170m, unknown/None) is SDR-by-default.
    let trc = info.color_transfer.as_deref().unwrap_or("");
    let prim = info.color_primaries.as_deref().unwrap_or("");
    let looks_hdr =
        matches!(trc, "smpte2084" | "arib-std-b67") || prim.eq_ignore_ascii_case("bt2020");
    // When color tags are entirely missing we don't know — probe to be safe.
    let no_color_info = info.color_transfer.is_none() && info.color_primaries.is_none();
    looks_hdr || no_color_info
}

/// Probe a file inside an ISO with the default timeout.
/// Convenience wrapper retained for symmetry with `probe_file`.
#[allow(dead_code)]
pub fn probe_iso_file(iso_path: &Path, inner_path: &str) -> Result<MediaInfo> {
    probe_iso_file_with_timeout(iso_path, inner_path, DEFAULT_PROBE_TIMEOUT)
}

/// Probe a file inside an ISO by streaming its contents to ffprobe via stdin.
/// Wrapped with the same watchdog as `probe_file_with_timeout` — an
/// unresponsive disc-image source must not be allowed to hang forever.
pub fn probe_iso_file_with_timeout(
    iso_path: &Path,
    inner_path: &str,
    timeout: Duration,
) -> Result<MediaInfo> {
    let mut child = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_streams",
            "-show_format",
            "-i",
            "pipe:0",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn ffprobe")?;

    let mut stdin = child.stdin.take().unwrap();

    // Stream ISO contents to ffprobe in a thread (ffprobe may close stdin early)
    let iso = iso_path.to_path_buf();
    let inner = inner_path.to_string();
    let writer_handle = std::thread::spawn(move || {
        let _ = crate::iso::cat_file(&iso, &inner, &mut stdin);
    });

    let descriptor = format!("{}:{}", iso_path.display(), inner_path);
    let output = wait_with_timeout(child, timeout, &descriptor)?;
    let _ = writer_handle.join();

    if !output.status.success() {
        bail!(
            "ffprobe failed for {}:{}: {}",
            iso_path.display(),
            inner_path,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // ISO sources don't get the side-data frame probe — it would require
    // streaming the whole inner file twice. Basic color tags from -show_streams
    // are usually enough for DVD/Blu-ray (HDR10 BDs are rare in raw-ISO form
    // and ffprobe-from-pipe doesn't always surface frame side-data anyway).
    parse_ffprobe_json(&output.stdout)
}

/// Spawn an arbitrary command with the watchdog and return its captured
/// output. Test-only helper: lets us drive the timeout logic without
/// shelling out to ffprobe (e.g. with `sleep` or `echo`).
#[cfg(test)]
fn run_with_timeout(
    program: &str,
    args: &[&str],
    timeout: Duration,
    descriptor: &str,
) -> Result<std::process::Output> {
    let child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to spawn {}", program))?;
    wait_with_timeout(child, timeout, descriptor)
}

/// Run ffprobe to read the first video frame's side-data list. Used to extract
/// HDR10 mastering display and content-light-level metadata, which only appears
/// on frames, not on the stream header.
fn probe_first_frame_side_data(path: &Path, timeout: Duration) -> Result<FfprobeFramesOutput> {
    let child = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-select_streams",
            "v:0",
            "-show_frames",
            "-read_intervals",
            "%+#1",
        ])
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn ffprobe (frames)")?;

    let descriptor = path.display().to_string();
    let output = wait_with_timeout(child, timeout, &descriptor)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let err_msg = stderr
            .lines()
            .rfind(|l| !l.trim().is_empty())
            .map(|s| s.to_string())
            .or_else(|| {
                let stdout = String::from_utf8_lossy(&output.stdout);
                stdout
                    .lines()
                    .rfind(|l| !l.trim().is_empty())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| format_exit_status(output.status));
        bail!("ffprobe (frames) failed for {:?}: {}", path, err_msg);
    }

    parse_frames_json(&output.stdout)
}

fn parse_frames_json(json: &[u8]) -> Result<FfprobeFramesOutput> {
    serde_json::from_slice(json).context("Failed to parse ffprobe frames JSON")
}

/// Pull MasteringDisplayMetadata + ContentLightLevelMetadata out of the first
/// frame's side-data list and stash the formatted strings on `info`.
fn apply_frame_side_data(info: &mut MediaInfo, frames: &FfprobeFramesOutput) {
    let Some(frame) = frames.frames.first() else {
        return;
    };
    let Some(side_data) = frame.side_data_list.as_ref() else {
        return;
    };
    for sd in side_data {
        match sd.side_data_type.as_deref() {
            // Only fill missing fields. Stream-header values already on
            // `info` are authoritative — we don't want first-frame
            // side-data quietly overwriting them with a different
            // mastering display reading.
            Some("Mastering display metadata") if info.master_display.is_none() => {
                if let Some(s) = format_master_display(sd) {
                    info.master_display = Some(s);
                }
            }
            Some("Content light level metadata") if info.max_cll.is_none() => {
                if let Some(s) = format_max_cll(sd) {
                    info.max_cll = Some(s);
                }
            }
            _ => {}
        }
    }
}

/// Parse a rational string like "13250/50000" into integer (numerator, denominator).
/// Returns None if either side is missing or non-numeric.
fn parse_rational(s: &str) -> Option<(i64, i64)> {
    let (num, den) = s.split_once('/')?;
    let num: i64 = num.trim().parse().ok()?;
    let den: i64 = den.trim().parse().ok()?;
    if den == 0 {
        return None;
    }
    Some((num, den))
}

/// Normalise a color rational ("13250/50000") to integer 1/50000 units, the
/// scale ffmpeg's `-master_display` flag expects for chromaticity coords.
fn rational_to_50000(s: &str) -> Option<i64> {
    let (num, den) = parse_rational(s)?;
    // Multiply first to keep precision: num * 50000 / den.
    Some(num.saturating_mul(50000) / den)
}

/// Normalise a luminance rational to integer 1/10000 nits, ffmpeg's expected
/// scale for `-master_display`'s L(max,min).
fn rational_to_10000(s: &str) -> Option<i64> {
    let (num, den) = parse_rational(s)?;
    Some(num.saturating_mul(10000) / den)
}

/// Build the master_display arg string from a MasteringDisplayMetadata side-data
/// entry. ffmpeg accepts the syntax
/// `G(gx,gy)B(bx,by)R(rx,ry)WP(wpx,wpy)L(max,min)` with chromaticity in
/// 1/50000 and luminance in 1/10000 nits. Returns None if any required field
/// is missing.
fn format_master_display(sd: &FfprobeSideData) -> Option<String> {
    let gx = rational_to_50000(sd.green_x.as_deref()?)?;
    let gy = rational_to_50000(sd.green_y.as_deref()?)?;
    let bx = rational_to_50000(sd.blue_x.as_deref()?)?;
    let by = rational_to_50000(sd.blue_y.as_deref()?)?;
    let rx = rational_to_50000(sd.red_x.as_deref()?)?;
    let ry = rational_to_50000(sd.red_y.as_deref()?)?;
    let wpx = rational_to_50000(sd.white_point_x.as_deref()?)?;
    let wpy = rational_to_50000(sd.white_point_y.as_deref()?)?;
    let lmax = rational_to_10000(sd.max_luminance.as_deref()?)?;
    let lmin = rational_to_10000(sd.min_luminance.as_deref()?)?;
    Some(format!(
        "G({},{})B({},{})R({},{})WP({},{})L({},{})",
        gx, gy, bx, by, rx, ry, wpx, wpy, lmax, lmin
    ))
}

/// Build the max_cll arg string ("max_cll,max_fall") from a
/// ContentLightLevelMetadata side-data entry. Returns None if either value
/// is missing.
fn format_max_cll(sd: &FfprobeSideData) -> Option<String> {
    let cll = sd.max_content?;
    let fall = sd.max_average?;
    Some(format!("{},{}", cll, fall))
}

fn parse_ffprobe_json(json: &[u8]) -> Result<MediaInfo> {
    let probe: FfprobeOutput =
        serde_json::from_slice(json).context("Failed to parse ffprobe JSON output")?;

    let video_stream = probe
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("video"))
        .context("No video stream found")?;

    let codec = video_stream
        .codec_name
        .clone()
        .unwrap_or_else(|| "unknown".to_string());

    let width = video_stream.width.unwrap_or(0);
    let height = video_stream.height.unwrap_or(0);

    let pix_fmt = video_stream
        .pix_fmt
        .clone()
        .unwrap_or_else(|| "unknown".to_string());

    // Try to get bitrate from stream tags first, then from format
    let bitrate_kbps = video_stream
        .tags
        .as_ref()
        .and_then(|t| t.bps.as_ref())
        .and_then(|b| b.parse::<u64>().ok())
        .map(|b| (b / 1000) as u32)
        .or_else(|| {
            probe
                .format
                .as_ref()
                .and_then(|f| f.bit_rate.as_ref())
                .and_then(|b| b.parse::<u64>().ok())
                .map(|b| (b / 1000) as u32)
        })
        .unwrap_or(0);

    let duration_secs = probe
        .format
        .as_ref()
        .and_then(|f| f.duration.as_ref())
        .and_then(|d| d.parse::<f64>().ok())
        .unwrap_or(0.0);

    let has_audio = probe
        .streams
        .iter()
        .any(|s| s.codec_type.as_deref() == Some("audio"));

    let has_subtitles = probe
        .streams
        .iter()
        .any(|s| s.codec_type.as_deref() == Some("subtitle"));

    // Color metadata from the video stream header. ffprobe emits "unknown" for
    // unspecified fields — treat that as None so we don't pass a useless
    // `-color_primaries unknown` to ffmpeg.
    let color_primaries = clean_color_tag(video_stream.color_primaries.as_deref());
    let color_transfer = clean_color_tag(video_stream.color_transfer.as_deref());
    let color_space = clean_color_tag(video_stream.color_space.as_deref());
    let color_range = clean_color_tag(video_stream.color_range.as_deref());

    // Some sources expose mastering display metadata directly in the stream
    // header (rare, but possible — e.g. some MKVs). Pick those up too.
    let mut master_display: Option<String> = None;
    let mut max_cll: Option<String> = None;
    if let Some(side_data) = video_stream.side_data_list.as_ref() {
        for sd in side_data {
            match sd.side_data_type.as_deref() {
                Some("Mastering display metadata") if master_display.is_none() => {
                    master_display = format_master_display(sd);
                }
                Some("Content light level metadata") if max_cll.is_none() => {
                    max_cll = format_max_cll(sd);
                }
                _ => {}
            }
        }
    }

    Ok(MediaInfo {
        codec,
        width,
        height,
        bitrate_kbps,
        duration_secs,
        pix_fmt,
        has_audio,
        has_subtitles,
        color_primaries,
        color_transfer,
        color_space,
        color_range,
        master_display,
        max_cll,
    })
}

/// Filter ffprobe's color tags: drop `None`, `"unknown"`, and empty strings.
/// ffprobe emits "unknown" for fields the bitstream didn't specify; passing
/// that to ffmpeg is worse than passing nothing.
fn clean_color_tag(s: Option<&str>) -> Option<String> {
    let s = s?.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("unknown") || s.eq_ignore_ascii_case("unspecified") {
        return None;
    }
    Some(s.to_string())
}

/// Check if a file already meets the target encoding requirements.
/// Returns true if the file should be skipped (already good enough).
pub fn meets_target(info: &MediaInfo, target: &TargetConfig) -> bool {
    // Must be h265/hevc
    let is_hevc = matches!(info.codec.as_str(), "hevc" | "h265");
    if !is_hevc {
        return false;
    }

    // Resolution must be at or below target
    if info.width > target.max_width || info.height > target.max_height {
        return false;
    }

    // If max_bitrate is set, check that too
    if target.max_bitrate_kbps > 0 && info.bitrate_kbps > target.max_bitrate_kbps {
        return false;
    }

    true
}

/// Returns true if the pixel format is 10-bit (or higher).
pub fn is_10bit(pix_fmt: &str) -> bool {
    pix_fmt.contains("10le")
        || pix_fmt.contains("10be")
        || pix_fmt.contains("12le")
        || pix_fmt.contains("12be")
        || pix_fmt == "p010"
        || pix_fmt == "p010le"
        || pix_fmt == "p010be"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_target() -> TargetConfig {
        TargetConfig {
            codec: "hevc".to_string(),
            quality: 28,
            preset: "slow".to_string(),
            max_width: 3840,
            max_height: 2160,
            max_bitrate_kbps: 0,
            container: "mkv".to_string(),
            audio_codec: "copy".to_string(),
            subtitle_codec: "copy".to_string(),
        }
    }

    fn make_info(codec: &str, width: u32, height: u32, bitrate_kbps: u32) -> MediaInfo {
        MediaInfo {
            codec: codec.to_string(),
            width,
            height,
            bitrate_kbps,
            duration_secs: 420.0,
            pix_fmt: "yuv420p".to_string(),
            has_audio: true,
            has_subtitles: false,
            ..MediaInfo::default()
        }
    }

    #[test]
    fn test_meets_target_hevc_within_bounds() {
        assert!(meets_target(
            &make_info("hevc", 1280, 720, 800),
            &make_target()
        ));
    }

    #[test]
    fn test_fails_target_not_hevc() {
        assert!(!meets_target(
            &make_info("h264", 1280, 720, 800),
            &make_target()
        ));
    }

    #[test]
    fn test_fails_target_too_large() {
        assert!(!meets_target(
            &make_info("hevc", 7680, 4320, 800),
            &make_target()
        ));
    }

    #[test]
    fn test_fails_target_bitrate_too_high() {
        let mut target = make_target();
        target.max_bitrate_kbps = 500;
        assert!(!meets_target(&make_info("hevc", 1280, 720, 800), &target));
    }

    #[test]
    fn test_meets_target_bitrate_within_limit() {
        let mut target = make_target();
        target.max_bitrate_kbps = 1000;
        assert!(meets_target(&make_info("hevc", 1280, 720, 800), &target));
    }

    #[test]
    fn test_is_10bit() {
        assert!(is_10bit("yuv420p10le"));
        assert!(is_10bit("p010le"));
        assert!(is_10bit("yuv444p10be"));
        assert!(!is_10bit("yuv420p"));
        assert!(!is_10bit("nv12"));
    }

    // ── Watchdog timeout tests ───────────────────────────────────────────────
    //
    // These exercise wait_with_timeout via the run_with_timeout test helper,
    // using stand-in commands (`sleep`, `echo`) so we don't depend on ffprobe
    // or any media files being present in the test environment.

    #[test]
    fn test_timeout_fires_on_slow_command() {
        // sleep 60 — way longer than the 1s watchdog. Must abort fast.
        let started = Instant::now();
        let result = run_with_timeout(
            "sleep",
            &["60"],
            Duration::from_secs(1),
            "/fake/path/that/hangs",
        );
        let elapsed = started.elapsed();

        assert!(result.is_err(), "expected timeout error, got {:?}", result);
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("timed out after 1s"),
            "error should mention timeout duration: {}",
            err
        );
        assert!(
            err.contains("/fake/path/that/hangs"),
            "error should include the descriptor path: {}",
            err
        );
        assert!(
            err.contains("filesystem may be unresponsive"),
            "error should hint at the likely cause: {}",
            err
        );
        // Watchdog must terminate close to the configured timeout, not 60s.
        // 5s upper bound leaves slack for slow CI hosts but rules out the
        // 60s sleep actually completing.
        assert!(
            elapsed < Duration::from_secs(5),
            "watchdog took {:?}, expected < 5s",
            elapsed
        );
    }

    #[test]
    fn test_timeout_does_not_fire_on_fast_command() {
        // echo finishes effectively instantly; a 5s timeout must not fire.
        let result = run_with_timeout("echo", &["{}"], Duration::from_secs(5), "fast-command");
        let output = result.expect("fast command should succeed");
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("{}"),
            "echo output missing payload: {}",
            stdout
        );
    }

    #[test]
    fn test_timeout_kills_child_on_expiry() {
        // The point of the watchdog is that a stuck probe can't keep the
        // process going forever. With a 200ms timeout against `sleep 30`,
        // wait_with_timeout must return an Err quickly (well under 1s,
        // accounting for the 500ms kill-reap grace window).
        let started = Instant::now();
        let result = run_with_timeout("sleep", &["30"], Duration::from_millis(200), "kill-test");
        let elapsed = started.elapsed();
        assert!(result.is_err(), "expected timeout error, got: {:?}", result);
        assert!(
            elapsed < Duration::from_secs(2),
            "kill path took too long: {:?}",
            elapsed
        );
    }

    #[test]
    fn test_timeout_message_renders_subsecond_durations() {
        let result = run_with_timeout("sleep", &["10"], Duration::from_millis(150), "subsec-test");
        let err = result.expect_err("expected timeout").to_string();
        // The error must mention the actual configured timeout, not "0s".
        assert!(
            err.contains("150ms"),
            "expected '150ms' in timeout error, got: {}",
            err
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_large_stdout_does_not_deadlock() {
        // Linux/macOS pipe buffer is typically 64 KB. A child writing >64 KB
        // to stdout stalls waiting for the reader, keeping try_wait() from
        // ever seeing it exit — the pre-fix polling loop would hit the
        // watchdog and report a spurious timeout. 96 KB is comfortably above
        // the buffer without leaning on macOS's larger pipe-resize behavior.
        //
        // `run_with_timeout` bails on watchdog fire, so `.expect()` alone
        // catches a regression — no separate elapsed assertion needed.
        let result = run_with_timeout(
            "dd",
            &["if=/dev/zero", "bs=98304", "count=1"],
            Duration::from_secs(5),
            "pipe-buffer-test",
        );
        let output = result.expect("96 KB stdout must complete without a timeout");
        assert_eq!(
            output.stdout.len(),
            98304,
            "must receive all 96 KB of stdout"
        );
    }

    #[test]
    fn test_exit_status_message_when_no_output() {
        // When a command exits non-zero with no stderr, the error message
        // must describe the exit code rather than saying "unknown error".
        let result = run_with_timeout("false", &[], Duration::from_secs(5), "exit-test");
        let output = result.expect("false exits immediately");
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        let msg = format_exit_status(output.status);
        assert!(
            msg.contains("exit code 1"),
            "expected 'exit code 1', got: {msg}"
        );
        assert!(
            msg.contains("no output"),
            "expected '(no output)' annotation, got: {msg}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_exit_status_message_on_signal() {
        // A process killed by a signal (SIGKILL = 9) must show the signal
        // number in the error message, not a bare exit code.
        use std::os::unix::process::ExitStatusExt;
        // Synthesise a fake ExitStatus that looks like SIGKILL (signal 9).
        // We do this by spawning a real process and killing it ourselves so
        // we get a genuine ExitStatus from the OS, not a mock.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("sleep should spawn");
        child.kill().expect("kill should succeed");
        let status = child.wait().expect("wait should succeed");
        assert!(
            status.signal().is_some(),
            "killed process should have a signal"
        );
        let msg = format_exit_status(status);
        assert!(
            msg.contains("signal"),
            "expected 'signal' in message, got: {msg}"
        );
        assert!(
            msg.contains("no output"),
            "expected '(no output)' annotation, got: {msg}"
        );
    }

    // ── HDR / color metadata tests ───────────────────────────────────────────

    #[test]
    fn test_clean_color_tag_drops_unknown() {
        assert_eq!(clean_color_tag(Some("bt2020")), Some("bt2020".to_string()));
        assert_eq!(clean_color_tag(Some("unknown")), None);
        assert_eq!(clean_color_tag(Some("UNKNOWN")), None);
        assert_eq!(clean_color_tag(Some("unspecified")), None);
        assert_eq!(clean_color_tag(Some("")), None);
        assert_eq!(clean_color_tag(Some("   ")), None);
        assert_eq!(clean_color_tag(None), None);
    }

    #[test]
    fn test_parse_color_tags_from_stream() {
        let json = br#"{
            "streams": [{
                "codec_name": "hevc",
                "codec_type": "video",
                "width": 3840,
                "height": 2160,
                "pix_fmt": "yuv420p10le",
                "color_primaries": "bt2020",
                "color_transfer": "smpte2084",
                "color_space": "bt2020nc",
                "color_range": "tv"
            }]
        }"#;
        let info = parse_ffprobe_json(json).unwrap();
        assert_eq!(info.color_primaries.as_deref(), Some("bt2020"));
        assert_eq!(info.color_transfer.as_deref(), Some("smpte2084"));
        assert_eq!(info.color_space.as_deref(), Some("bt2020nc"));
        assert_eq!(info.color_range.as_deref(), Some("tv"));
    }

    #[test]
    fn test_parse_unknown_color_tags_become_none() {
        let json = br#"{
            "streams": [{
                "codec_name": "h264",
                "codec_type": "video",
                "width": 1920,
                "height": 1080,
                "pix_fmt": "yuv420p",
                "color_primaries": "unknown",
                "color_transfer": "unknown",
                "color_space": "unknown",
                "color_range": "unknown"
            }]
        }"#;
        let info = parse_ffprobe_json(json).unwrap();
        assert_eq!(info.color_primaries, None);
        assert_eq!(info.color_transfer, None);
        assert_eq!(info.color_space, None);
        assert_eq!(info.color_range, None);
    }

    #[test]
    fn test_master_display_format_from_stream_side_data() {
        // BT.2020 primaries with a 1000-nit / 0.005-nit display.
        // Coordinates expressed as ffprobe does: rationals over 50000 / 10000.
        let json = br#"{
            "streams": [{
                "codec_name": "hevc",
                "codec_type": "video",
                "width": 3840,
                "height": 2160,
                "pix_fmt": "yuv420p10le",
                "color_primaries": "bt2020",
                "color_transfer": "smpte2084",
                "color_space": "bt2020nc",
                "side_data_list": [{
                    "side_data_type": "Mastering display metadata",
                    "red_x": "35400/50000",
                    "red_y": "14600/50000",
                    "green_x": "8500/50000",
                    "green_y": "39850/50000",
                    "blue_x": "6550/50000",
                    "blue_y": "2300/50000",
                    "white_point_x": "15635/50000",
                    "white_point_y": "16450/50000",
                    "min_luminance": "50/10000",
                    "max_luminance": "10000000/10000"
                }, {
                    "side_data_type": "Content light level metadata",
                    "max_content": 1000,
                    "max_average": 400
                }]
            }]
        }"#;
        let info = parse_ffprobe_json(json).unwrap();
        assert_eq!(
            info.master_display.as_deref(),
            Some("G(8500,39850)B(6550,2300)R(35400,14600)WP(15635,16450)L(10000000,50)")
        );
        assert_eq!(info.max_cll.as_deref(), Some("1000,400"));
    }

    #[test]
    fn test_master_display_format_from_frame_side_data() {
        // When mastering metadata only appears on frames (the typical HDR10 case),
        // probe_first_frame_side_data returns it; apply_frame_side_data merges it in.
        let frames_json = br#"{
            "frames": [{
                "side_data_list": [{
                    "side_data_type": "Mastering display metadata",
                    "red_x": "34000/50000",
                    "red_y": "16000/50000",
                    "green_x": "13250/50000",
                    "green_y": "34500/50000",
                    "blue_x": "7500/50000",
                    "blue_y": "3000/50000",
                    "white_point_x": "15635/50000",
                    "white_point_y": "16450/50000",
                    "min_luminance": "1/10000",
                    "max_luminance": "40000000/10000"
                }, {
                    "side_data_type": "Content light level metadata",
                    "max_content": 4000,
                    "max_average": 1000
                }]
            }]
        }"#;
        let frames = parse_frames_json(frames_json).unwrap();
        let mut info = MediaInfo::default();
        apply_frame_side_data(&mut info, &frames);
        assert_eq!(
            info.master_display.as_deref(),
            Some("G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(40000000,1)")
        );
        assert_eq!(info.max_cll.as_deref(), Some("4000,1000"));
    }

    #[test]
    fn test_master_display_missing_field_returns_none() {
        // Drop one required field — formatter must refuse rather than emit garbage.
        let frames_json = br#"{
            "frames": [{
                "side_data_list": [{
                    "side_data_type": "Mastering display metadata",
                    "red_x": "34000/50000",
                    "red_y": "16000/50000",
                    "green_x": "13250/50000",
                    "green_y": "34500/50000",
                    "blue_x": "7500/50000",
                    "white_point_x": "15635/50000",
                    "white_point_y": "16450/50000",
                    "min_luminance": "1/10000",
                    "max_luminance": "40000000/10000"
                }]
            }]
        }"#;
        let frames = parse_frames_json(frames_json).unwrap();
        let mut info = MediaInfo::default();
        apply_frame_side_data(&mut info, &frames);
        assert_eq!(info.master_display, None);
    }

    #[test]
    fn test_max_cll_missing_field_returns_none() {
        let frames_json = br#"{
            "frames": [{
                "side_data_list": [{
                    "side_data_type": "Content light level metadata",
                    "max_content": 1000
                }]
            }]
        }"#;
        let frames = parse_frames_json(frames_json).unwrap();
        let mut info = MediaInfo::default();
        apply_frame_side_data(&mut info, &frames);
        assert_eq!(info.max_cll, None);
    }

    #[test]
    fn test_apply_frame_side_data_does_not_overwrite_stream_header() {
        // Stream-header parsing already populated master_display; the
        // first-frame side-data must not silently replace it (the stream
        // header is the authoritative reading; frame side-data here is
        // a fallback for sources that lack stream-header HDR tags).
        let frames_json = br#"{
            "frames": [{
                "side_data_list": [{
                    "side_data_type": "Mastering display metadata",
                    "red_x": "34000/50000",
                    "red_y": "16000/50000",
                    "green_x": "13250/50000",
                    "green_y": "34500/50000",
                    "blue_x": "7500/50000",
                    "blue_y": "3000/50000",
                    "white_point_x": "15635/50000",
                    "white_point_y": "16450/50000",
                    "min_luminance": "1/10000",
                    "max_luminance": "40000000/10000"
                }]
            }]
        }"#;
        let frames = parse_frames_json(frames_json).unwrap();
        let mut info = MediaInfo::default();
        info.master_display = Some("STREAM-HEADER-VALUE".to_string());
        apply_frame_side_data(&mut info, &frames);
        assert_eq!(info.master_display.as_deref(), Some("STREAM-HEADER-VALUE"));
    }

    #[test]
    fn test_needs_frame_side_data_probe_skips_complete_sdr() {
        // SDR file with full stream-header coverage — never probe frames.
        let mut info = MediaInfo::default();
        info.color_transfer = Some("bt709".to_string());
        info.color_primaries = Some("bt709".to_string());
        info.master_display = Some("x".to_string());
        info.max_cll = Some("y".to_string());
        assert!(!needs_frame_side_data_probe(&info));
    }

    #[test]
    fn test_needs_frame_side_data_probe_skips_sdr_missing_hdr_fields() {
        // SDR file with no HDR fields — also no point probing frames;
        // frame side-data on SDR is empty.
        let mut info = MediaInfo::default();
        info.color_transfer = Some("bt709".to_string());
        info.color_primaries = Some("bt709".to_string());
        assert!(!needs_frame_side_data_probe(&info));
    }

    #[test]
    fn test_needs_frame_side_data_probe_runs_on_pq_hdr_missing_fields() {
        let mut info = MediaInfo::default();
        info.color_transfer = Some("smpte2084".to_string());
        info.color_primaries = Some("bt2020".to_string());
        // master_display / max_cll empty → must probe to fill them.
        assert!(needs_frame_side_data_probe(&info));
    }

    #[test]
    fn test_needs_frame_side_data_probe_runs_when_color_tags_unknown() {
        // Source with no color metadata at all: we don't know if it's
        // HDR or SDR, so probe to be safe.
        let info = MediaInfo::default();
        assert!(needs_frame_side_data_probe(&info));
    }

    #[test]
    fn test_needs_frame_side_data_probe_skips_pq_when_already_populated() {
        let mut info = MediaInfo::default();
        info.color_transfer = Some("smpte2084".to_string());
        info.master_display = Some("x".to_string());
        info.max_cll = Some("y".to_string());
        assert!(!needs_frame_side_data_probe(&info));
    }

    #[test]
    fn test_rational_helpers() {
        assert_eq!(rational_to_50000("13250/50000"), Some(13250));
        assert_eq!(rational_to_50000("1/2"), Some(25000));
        assert_eq!(rational_to_50000("garbage"), None);
        assert_eq!(rational_to_50000("1/0"), None);
        assert_eq!(rational_to_10000("40000000/10000"), Some(40000000));
        assert_eq!(rational_to_10000("1/10000"), Some(1));
    }
}
