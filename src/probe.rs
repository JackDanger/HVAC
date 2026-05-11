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
    let started = Instant::now();
    loop {
        match child.try_wait().context("Failed to poll ffprobe child")? {
            Some(_status) => {
                // Process exited — collect its captured stdout/stderr.
                return child
                    .wait_with_output()
                    .context("Failed to collect ffprobe output");
            }
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
    }
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

#[derive(Debug, Clone)]
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

/// Probe a file with the default timeout. Convenience wrapper for callers
/// that don't have a configured `Duration` (e.g. transcode-output validation).
pub fn probe_file(path: &Path) -> Result<MediaInfo> {
    probe_file_with_timeout(path, DEFAULT_PROBE_TIMEOUT)
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
            .unwrap_or("unknown error");
        bail!("ffprobe failed for {:?}: {}", path, err_msg);
    }

    parse_ffprobe_json(&output.stdout)
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

    Ok(MediaInfo {
        codec,
        width,
        height,
        bitrate_kbps,
        duration_secs,
        pix_fmt,
        has_audio,
        has_subtitles,
    })
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
}
