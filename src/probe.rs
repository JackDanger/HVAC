use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::process::Command;

use crate::config::TargetConfig;

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

pub fn probe_file(path: &Path) -> Result<MediaInfo> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_streams",
            "-show_format",
        ])
        .arg(path)
        .output()
        .context("Failed to run ffprobe")?;

    if !output.status.success() {
        bail!(
            "ffprobe failed for {:?}: {}",
            path,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let probe: FfprobeOutput =
        serde_json::from_slice(&output.stdout).context("Failed to parse ffprobe JSON output")?;

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
}
