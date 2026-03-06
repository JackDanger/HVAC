use anyhow::{bail, Result};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct GpuInfo {
    pub name: String,
    pub encoder: String,
    pub kind: GpuKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GpuKind {
    Nvidia,
    Intel,
    Apple,
}

/// Detect available GPU for h265 encoding.
/// Checks NVIDIA first (hevc_nvenc), then Intel (hevc_vaapi).
/// Exits with a clear error if no GPU is found.
pub fn detect_gpu() -> Result<GpuInfo> {
    // Check for NVIDIA GPU via nvidia-smi
    if let Ok(nvidia) = detect_nvidia() {
        // Verify ffmpeg has hevc_nvenc
        if has_ffmpeg_encoder("hevc_nvenc") {
            return Ok(GpuInfo {
                name: nvidia,
                encoder: "hevc_nvenc".to_string(),
                kind: GpuKind::Nvidia,
            });
        }
        log::warn!("NVIDIA GPU found but hevc_nvenc not available in ffmpeg");
    }

    // Check for Intel GPU via vainfo or /dev/dri
    if detect_intel_gpu() {
        if has_ffmpeg_encoder("hevc_vaapi") {
            return Ok(GpuInfo {
                name: "Intel GPU (VAAPI)".to_string(),
                encoder: "hevc_vaapi".to_string(),
                kind: GpuKind::Intel,
            });
        }
        log::warn!("Intel GPU found but hevc_vaapi not available in ffmpeg");
    }

    // Check for Apple VideoToolbox (macOS)
    if detect_apple_gpu() {
        if has_ffmpeg_encoder("hevc_videotoolbox") {
            return Ok(GpuInfo {
                name: detect_apple_chip_name(),
                encoder: "hevc_videotoolbox".to_string(),
                kind: GpuKind::Apple,
            });
        }
        log::warn!("Apple GPU found but hevc_videotoolbox not available in ffmpeg");
    }

    bail!(
        "No GPU found for h265 encoding!\n\
         tdorr requires one of:\n\
         - NVIDIA GPU with NVENC support (hevc_nvenc)\n\
         - Intel GPU with VAAPI support (hevc_vaapi)\n\
         - Apple Silicon or Mac with VideoToolbox (hevc_videotoolbox)\n\
         \n\
         Check that:\n\
         1. A supported GPU is installed\n\
         2. Drivers are loaded (nvidia-smi, vainfo, or macOS)\n\
         3. ffmpeg is built with the appropriate encoder"
    )
}

fn detect_nvidia() -> Result<String> {
    let output = Command::new("nvidia-smi")
        .arg("--query-gpu=name")
        .arg("--format=csv,noheader")
        .output()?;

    if !output.status.success() {
        bail!("nvidia-smi failed");
    }

    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if name.is_empty() {
        bail!("no nvidia gpu found");
    }
    Ok(name)
}

fn detect_intel_gpu() -> bool {
    // Check for /dev/dri/renderD128 (Intel VAAPI)
    std::path::Path::new("/dev/dri/renderD128").exists()
}

fn detect_apple_gpu() -> bool {
    cfg!(target_os = "macos")
}

fn detect_apple_chip_name() -> String {
    let output = Command::new("sysctl")
        .arg("-n")
        .arg("machdep.cpu.brand_string")
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !name.is_empty() {
                return format!("Apple ({})", name);
            }
        }
        _ => {}
    }
    "Apple GPU (VideoToolbox)".to_string()
}

fn has_ffmpeg_encoder(encoder: &str) -> bool {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output();

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            stdout.contains(encoder)
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_has_ffmpeg_encoder() {
        // This test runs on the remote host where ffmpeg is available
        // At minimum, libx265 should be available
        let has_any = has_ffmpeg_encoder("libx265")
            || has_ffmpeg_encoder("hevc_nvenc")
            || has_ffmpeg_encoder("hevc_vaapi")
            || has_ffmpeg_encoder("hevc_videotoolbox");
        assert!(has_any, "ffmpeg should have at least one h265 encoder");
    }

    #[test]
    fn test_detect_gpu_returns_result() {
        // This should succeed on the remote host (RTX 2060)
        // or fail with a clear message - either way it shouldn't panic
        let result = detect_gpu();
        match &result {
            Ok(gpu) => {
                assert!(!gpu.name.is_empty());
                assert!(!gpu.encoder.is_empty());
                println!("Detected GPU: {:?}", gpu);
            }
            Err(e) => {
                println!("No GPU detected (expected in CI): {}", e);
            }
        }
    }
}
