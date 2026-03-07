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
        log::debug!("NVIDIA GPU found but hevc_nvenc not available in ffmpeg");
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
        log::debug!("Intel GPU found but hevc_vaapi not available in ffmpeg");
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
        log::debug!("Apple GPU found but hevc_videotoolbox not available in ffmpeg");
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

/// Return the maximum number of simultaneous encode sessions this GPU supports.
/// Used as the default for --jobs.
pub fn max_encode_sessions(gpu: &GpuInfo) -> usize {
    match gpu.kind {
        GpuKind::Nvidia => {
            // Professional GPUs (Quadro, Tesla, A-series) have no session limit.
            // Consumer GeForce GPUs are limited to 3 sessions (driver 550.40+).
            let name_lower = gpu.name.to_lowercase();
            if name_lower.contains("quadro")
                || name_lower.contains("tesla")
                || name_lower.starts_with("a10")
                || name_lower.starts_with("a30")
                || name_lower.starts_with("a40")
                || name_lower.starts_with("a100")
                || name_lower.starts_with("l4")
                || name_lower.starts_with("l40")
                || name_lower.starts_with("h100")
            {
                4 // Professional: no hard limit, default to 4
            } else {
                3 // Consumer GeForce: 3 simultaneous NVENC sessions
            }
        }
        // VAAPI and VideoToolbox don't have hard session limits,
        // but diminishing returns past a few concurrent encodes
        GpuKind::Intel => 2,
        GpuKind::Apple => 2,
    }
}

/// Count active NVENC encode sessions by looking for running ffmpeg hevc_nvenc processes.
/// Returns the number of sessions used by OTHER processes (not our own).
pub fn active_nvenc_sessions() -> usize {
    let output = Command::new("ps").args(["ax", "-o", "pid,args"]).output();

    let own_pid = std::process::id();

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            stdout
                .lines()
                .filter(|line| {
                    line.contains("hevc_nvenc") && !line.contains("grep") && {
                        // Exclude our own child processes
                        line.split_whitespace()
                            .next()
                            .and_then(|pid| pid.parse::<u32>().ok())
                            .map(|pid| {
                                // Check if this process's parent is us
                                !is_child_of(pid, own_pid)
                            })
                            .unwrap_or(true)
                    }
                })
                .count()
        }
        Err(_) => 0,
    }
}

/// Check if `pid` is a child of `parent_pid` by reading /proc/pid/stat.
fn is_child_of(pid: u32, parent_pid: u32) -> bool {
    // On Linux, read /proc/pid/stat to find ppid
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // Format: pid (comm) state ppid ...
        // Find the closing ) then parse ppid
        if let Some(after_comm) = stat.rfind(')') {
            let fields: Vec<&str> = stat[after_comm + 2..].split_whitespace().collect();
            if let Some(ppid) = fields.get(1).and_then(|s| s.parse::<u32>().ok()) {
                return ppid == parent_pid;
            }
        }
    }
    // On macOS, fall back to assuming not a child
    false
}

/// Return available encode sessions (max minus active external sessions).
pub fn available_sessions(gpu: &GpuInfo) -> usize {
    let max = max_encode_sessions(gpu);
    if gpu.kind != GpuKind::Nvidia {
        return max;
    }
    let active = active_nvenc_sessions();
    if active > 0 {
        eprintln!(
            "Note: {} active NVENC sessions from other processes",
            active
        );
    }
    max.saturating_sub(active).max(1)
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
    fn test_max_encode_sessions_consumer_nvidia() {
        let gpu = GpuInfo {
            name: "NVIDIA GeForce RTX 2060".to_string(),
            encoder: "hevc_nvenc".to_string(),
            kind: GpuKind::Nvidia,
        };
        assert_eq!(max_encode_sessions(&gpu), 3);
    }

    #[test]
    fn test_max_encode_sessions_professional_nvidia() {
        let gpu = GpuInfo {
            name: "A100-SXM4-80GB".to_string(),
            encoder: "hevc_nvenc".to_string(),
            kind: GpuKind::Nvidia,
        };
        assert_eq!(max_encode_sessions(&gpu), 4);
    }

    #[test]
    fn test_active_nvenc_sessions_returns_number() {
        // Just verify it doesn't panic
        let count = active_nvenc_sessions();
        assert!(count < 100, "unreasonable session count: {count}");
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
