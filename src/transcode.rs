use anyhow::{bail, Context, Result};
use std::os::unix::fs::{chown, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::TargetConfig;
use crate::gpu::{GpuInfo, GpuKind};
use crate::probe;

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

/// Transcode a file using ffmpeg with GPU acceleration.
/// If `output` is None, transcode in-place (to a temp file, then replace original).
/// `source_bitrate_kbps` is used to cap the output so we never produce a larger file.
pub fn transcode(
    source: &Path,
    output: Option<&Path>,
    target: &TargetConfig,
    gpu: &GpuInfo,
    source_bitrate_kbps: u32,
    source_duration_secs: f64,
) -> Result<PathBuf> {
    let final_output = match output {
        Some(p) => p.to_path_buf(),
        None => {
            // In-place: use temp file then rename
            let parent = source.parent().context("source has no parent")?;
            parent.join(format!(
                ".tdorr_tmp_{}.{}",
                source
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy(),
                target.container
            ))
        }
    };

    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-y"]);

    // Input
    cmd.args(["-i"]).arg(source);

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
            // HEVC should always beat the source codec at the same bitrate.
            if source_bitrate_kbps > 0 {
                cmd.args(["-maxrate", &format!("{}k", source_bitrate_kbps)]);
                cmd.args(["-bufsize", &format!("{}k", source_bitrate_kbps * 2)]);
            }
            cmd.args(["-b:v", "0"]);
        }
        GpuKind::Intel => {
            cmd.args(["-vaapi_device", "/dev/dri/renderD128"]);
            cmd.args(["-c:v", "hevc_vaapi"]);
            cmd.args(["-global_quality", &target.quality.to_string()]);
            if source_bitrate_kbps > 0 {
                cmd.args(["-maxrate", &format!("{}k", source_bitrate_kbps)]);
                cmd.args(["-bufsize", &format!("{}k", source_bitrate_kbps * 2)]);
            }
            cmd.args(["-vf", "format=nv12,hwupload"]);
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
    cmd.args(["-map", "0:a?"]);  // all audio streams
    cmd.args(["-map", "0:s?"]);  // all subtitle streams

    // Output
    cmd.arg(&final_output);

    log::info!("Running: {:?}", cmd);

    let status = cmd
        .status()
        .context("Failed to execute ffmpeg")?;

    if !status.success() {
        let _ = std::fs::remove_file(&final_output);
        bail!("ffmpeg exited with status: {}", status);
    }

    // Validate the output before considering it done
    if let Err(e) = validate_output(&final_output, source, source_duration_secs) {
        let _ = std::fs::remove_file(&final_output);
        bail!("Output validation failed: {}", e);
    }

    // Copy permissions (user, group, mode) from source to output
    copy_permissions(source, &final_output)?;

    // If in-place mode, replace original only after validation passes
    if output.is_none() {
        std::fs::rename(&final_output, source)
            .context("Failed to replace original file with transcoded version")?;
        return Ok(source.to_path_buf());
    }

    Ok(final_output)
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
        log::warn!("Could not set owner/group on {:?}: {} (requires root)", dest, e);
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
}
