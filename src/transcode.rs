use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::TargetConfig;
use crate::gpu::{GpuInfo, GpuKind};

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
pub fn transcode(
    source: &Path,
    output: Option<&Path>,
    target: &TargetConfig,
    gpu: &GpuInfo,
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
            cmd.args(["-b:v", "0"]);
        }
        GpuKind::Intel => {
            cmd.args(["-vaapi_device", "/dev/dri/renderD128"]);
            cmd.args(["-c:v", "hevc_vaapi"]);
            cmd.args(["-global_quality", &target.quality.to_string()]);
            cmd.args(["-vf", "format=nv12,hwupload"]);
        }
    }

    // Audio
    cmd.args(["-c:a", &target.audio_codec]);

    // Subtitles
    if target.subtitle_codec == "copy" {
        cmd.args(["-c:s", "copy"]);
    }

    // Map all streams
    cmd.args(["-map", "0"]);

    // Output
    cmd.arg(&final_output);

    log::info!("Running: {:?}", cmd);

    let status = cmd
        .status()
        .context("Failed to execute ffmpeg")?;

    if !status.success() {
        // Clean up temp file on failure
        let _ = std::fs::remove_file(&final_output);
        bail!("ffmpeg exited with status: {}", status);
    }

    // If in-place mode, replace original
    if output.is_none() {
        std::fs::rename(&final_output, source)
            .context("Failed to replace original file with transcoded version")?;
        return Ok(source.to_path_buf());
    }

    Ok(final_output)
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
