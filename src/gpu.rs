use anyhow::{bail, Context, Result};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct GpuInfo {
    pub name: String,
    pub encoder: String,
    pub kind: GpuKind,
    /// Whether this GPU's HEVC encoder reliably supports 10-bit input.
    /// False for Maxwell-era and early Pascal NVENC silicon. True elsewhere.
    pub supports_10bit_hevc: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GpuKind {
    Nvidia,
    Intel,
    Apple,
}

/// NVIDIA GPU name fragments whose NVENC silicon does NOT reliably support
/// 10-bit HEVC encode.
///
///   - Maxwell 1st gen (GTX 750 / 750 Ti) has no HEVC encode at all. The
///     gate flag is "no 10-bit HEVC" today, but a separate "no HEVC at
///     all" gate is the right longer-term shape for these cards (8-bit
///     HEVC will still attempt and fail). Tracked as a follow-up; for now
///     skipping 10-bit is at least no worse than the prior opaque
///     mid-encode failure.
///   - Maxwell 2nd gen (GTX 9xx) does 8-bit HEVC only.
///   - Pascal: the GP104 / GP102 chips on GTX 1080, GTX 1080 Ti, and Titan
///     Xp do support 10-bit HEVC, but for V1 we list "GTX 1080" as a
///     conservative match anyway. This is intentionally pessimistic —
///     telling a user with a 1080 Ti to convert to 8-bit first is
///     annoying but not destructive; missing a card that genuinely can't
///     encode 10-bit is. The tests assert this conservative behavior.
///     Refining to per-chip detection is a follow-up.
///
/// Reference: <https://developer.nvidia.com/video-encode-and-decode-gpu-support-matrix-new>
///
/// Match is case-insensitive substring on `nvidia-smi --query-gpu=name`.
const NVENC_NO_10BIT_HEVC: &[&str] = &[
    // Maxwell 1st gen — no HEVC encode at all.
    "GTX 750",
    // Maxwell 2nd gen — 8-bit HEVC only.
    // Matches "GTX 950", "GTX 960", "GTX 970", "GTX 980", "GTX 980 Ti", etc.
    "GTX 9", // Early Pascal — conservatively gated on 10-bit; see module comment.
    "GTX 1050", "GTX 1060", "GTX 1070", "GTX 1080",
];

/// Returns true when the given NVIDIA GPU name matches a known
/// no-10-bit-HEVC NVENC family.
fn nvidia_supports_10bit_hevc(name: &str) -> bool {
    let lower = name.to_lowercase();
    for fragment in NVENC_NO_10BIT_HEVC {
        if lower.contains(&fragment.to_lowercase()) {
            return false;
        }
    }
    true
}

/// Detect available GPU for h265 encoding.
/// Checks NVIDIA first (hevc_nvenc), then Intel (hevc_vaapi).
/// Exits with a clear error if no GPU is found.
pub fn detect_gpu() -> Result<GpuInfo> {
    // Check for NVIDIA GPU via nvidia-smi
    if let Ok(nvidia) = detect_nvidia() {
        // Verify ffmpeg has hevc_nvenc
        if has_ffmpeg_encoder("hevc_nvenc") {
            let supports_10bit = nvidia_supports_10bit_hevc(&nvidia);
            return Ok(GpuInfo {
                name: nvidia,
                encoder: "hevc_nvenc".to_string(),
                kind: GpuKind::Nvidia,
                supports_10bit_hevc: supports_10bit,
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
                supports_10bit_hevc: true,
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
                supports_10bit_hevc: true,
            });
        }
        log::debug!("Apple GPU found but hevc_videotoolbox not available in ffmpeg");
    }

    bail!("{}", no_gpu_message())
}

/// Build the "no GPU found" message, branching on platform context so the
/// hints point at the right fix instead of dumping the full matrix on
/// everyone. Kept as a free function for inline tests — the message is
/// the first impression a brand-new user gets when their setup doesn't
/// match the README's GPU table, so we'd like to know if it regresses.
fn no_gpu_message() -> String {
    let mut msg = String::from(
        "No GPU found for h265 encoding!\n\
         hvac requires one of:\n\
         - NVIDIA GPU with NVENC support (hevc_nvenc)\n\
         - Intel GPU with VAAPI support (hevc_vaapi)\n\
         - Apple Silicon or Mac with VideoToolbox (hevc_videotoolbox)\n\n",
    );

    if cfg!(target_os = "macos") {
        // On macOS the encoder is built into the OS, so a "no GPU" bail
        // here almost always means ffmpeg is missing or was built without
        // VideoToolbox (rare, but `ffmpeg-free` clones exist).
        msg.push_str(
            "On macOS, VideoToolbox ships with the OS. This message usually means:\n\
             - ffmpeg is not installed:  brew install ffmpeg\n\
             - or a non-default ffmpeg build lacks hevc_videotoolbox; reinstall from Homebrew\n",
        );
    } else if running_in_container() {
        // Containerised runs almost always fail because the device node
        // wasn't passed through. Point at the exact docker flag.
        msg.push_str(
            "Detected a container environment. The GPU device wasn't passed through:\n\
             - Intel iGPU:   add `--device /dev/dri:/dev/dri` to your docker run\n\
             - NVIDIA:       add `--gpus all --runtime=nvidia` (needs nvidia-container-toolkit)\n\
             See https://github.com/JackDanger/hvac/blob/main/docs/NAS.md for the NAS-specific recipes.\n",
        );
    } else {
        msg.push_str(
            "Check that:\n\
             1. A supported GPU is installed\n\
             2. Drivers are loaded — `nvidia-smi` for NVIDIA, `ls /dev/dri && vainfo` for Intel\n\
             3. ffmpeg is built with the appropriate encoder (`ffmpeg -encoders | grep hevc_`)\n\n\
             Synology, QNAP, Unraid, OMV, TrueNAS: see\n\
             https://github.com/JackDanger/hvac/blob/main/docs/NAS.md\n",
        );
    }
    msg
}

/// Best-effort container detection. Used only to pick which set of hints
/// to show in the "no GPU" error — false positives just mean a Linux
/// host user sees the docker hint, which is harmless. False negatives
/// (real container, undetected) fall through to the generic Linux
/// hints, which is also fine.
fn running_in_container() -> bool {
    // Most container runtimes drop a /.dockerenv or /run/.containerenv
    // marker at the rootfs. Podman uses the latter; Docker the former.
    if std::path::Path::new("/.dockerenv").exists()
        || std::path::Path::new("/run/.containerenv").exists()
    {
        return true;
    }
    // Kubernetes-style runs sometimes have neither marker but always set
    // these env vars. Cheap belt-and-braces.
    std::env::var_os("KUBERNETES_SERVICE_HOST").is_some()
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

/// Run a command and collect its output, killing it if it hasn't finished
/// within `timeout`. Used for startup GPU/encoder probes whose output is
/// small enough that pipe-buffer overflow is not a concern, but which could
/// hang forever if a GPU driver or tool is unresponsive.
///
/// Spawns the child directly (not in a detached thread) so the timeout path
/// can `child.kill()` it instead of leaking the subprocess. Output pipes are
/// drained on background threads to defeat the same pipe-buffer-deadlock
/// `wait_with_timeout` solves for ffprobe.
fn run_output_with_timeout(
    mut cmd: Command,
    timeout: Duration,
    label: &str,
) -> Result<std::process::Output> {
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn {label}"))?;

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
        match child
            .try_wait()
            .with_context(|| format!("failed to poll {label}"))?
        {
            Some(status) => break status,
            None => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    // Joining now is safe: kill() closes stdin/stdout/stderr,
                    // so the reader threads observe EOF and exit promptly.
                    // Without these joins the threads are detached and may
                    // outlive the bail.
                    let _ = stdout_thread.map(|h| h.join());
                    let _ = stderr_thread.map(|h| h.join());
                    bail!("{label} timed out after {}s", timeout.as_secs());
                }
                std::thread::sleep(Duration::from_millis(50));
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

fn detect_nvidia() -> Result<String> {
    let mut cmd = Command::new("nvidia-smi");
    cmd.arg("--query-gpu=name")
        .arg("--format=csv,noheader")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = run_output_with_timeout(cmd, Duration::from_secs(10), "nvidia-smi")?;

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
    let mut cmd = Command::new("sysctl");
    cmd.arg("-n")
        .arg("machdep.cpu.brand_string")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    match run_output_with_timeout(cmd, Duration::from_secs(5), "sysctl") {
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
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-encoders"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    match run_output_with_timeout(cmd, Duration::from_secs(10), "ffmpeg") {
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
            supports_10bit_hevc: true,
        };
        assert_eq!(max_encode_sessions(&gpu), 3);
    }

    #[test]
    fn test_max_encode_sessions_professional_nvidia() {
        let gpu = GpuInfo {
            name: "A100-SXM4-80GB".to_string(),
            encoder: "hevc_nvenc".to_string(),
            kind: GpuKind::Nvidia,
            supports_10bit_hevc: true,
        };
        assert_eq!(max_encode_sessions(&gpu), 4);
    }

    // ── 10-bit HEVC capability mapping ──────────────────────────────────────
    //
    // String-match GPU names against the known no-10-bit families.
    // Anything not on the list (Turing+, Ampere, Ada, Hopper, Quadros)
    // is assumed to support 10-bit HEVC encode.

    #[test]
    fn test_10bit_unsupported_maxwell_first_gen() {
        // Maxwell 1st gen GTX 750 / 750 Ti — no HEVC at all.
        assert!(!nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 750"));
        assert!(!nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 750 Ti"));
    }

    #[test]
    fn test_10bit_unsupported_maxwell_second_gen() {
        // Maxwell 2nd gen GTX 9xx — 8-bit HEVC only.
        assert!(!nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 950"));
        assert!(!nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 960"));
        assert!(!nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 970"));
        assert!(!nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 980"));
        assert!(!nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 980 Ti"));
    }

    #[test]
    fn test_10bit_unsupported_early_pascal() {
        // Early Pascal — no 10-bit HEVC.
        assert!(!nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 1050"));
        assert!(!nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 1050 Ti"));
        assert!(!nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 1060 3GB"));
        assert!(!nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 1060 6GB"));
        assert!(!nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 1070"));
        assert!(!nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 1070 Ti"));
        assert!(!nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 1080"));
        assert!(!nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 1080 Ti"));
    }

    #[test]
    fn test_10bit_supported_turing_and_newer() {
        // Turing (RTX 20xx, GTX 16xx) — full 10-bit HEVC.
        assert!(nvidia_supports_10bit_hevc("NVIDIA GeForce RTX 2060"));
        assert!(nvidia_supports_10bit_hevc("NVIDIA GeForce RTX 2080 Ti"));
        assert!(nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 1660 Ti"));
        assert!(nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 1650"));
        // Ampere
        assert!(nvidia_supports_10bit_hevc("NVIDIA GeForce RTX 3060"));
        assert!(nvidia_supports_10bit_hevc("NVIDIA GeForce RTX 3090"));
        // Ada
        assert!(nvidia_supports_10bit_hevc("NVIDIA GeForce RTX 4090"));
        // Pro
        assert!(nvidia_supports_10bit_hevc("NVIDIA A100-SXM4-80GB"));
        assert!(nvidia_supports_10bit_hevc("Quadro RTX 5000"));
        assert!(nvidia_supports_10bit_hevc("Tesla T4"));
    }

    #[test]
    fn test_10bit_case_insensitive() {
        // Match must be case-insensitive — nvidia-smi formatting varies.
        assert!(!nvidia_supports_10bit_hevc("nvidia geforce gtx 970"));
        assert!(!nvidia_supports_10bit_hevc("GEFORCE GTX 1080"));
    }

    #[test]
    fn no_gpu_message_mentions_all_three_encoders() {
        // The body of the error has to keep naming the three encoders
        // hvac drives — that's the only place a brand-new user sees the
        // full matrix before going to look up their hardware.
        let m = no_gpu_message();
        assert!(m.contains("hevc_nvenc"), "missing nvenc mention: {}", m);
        assert!(m.contains("hevc_vaapi"), "missing vaapi mention: {}", m);
        assert!(
            m.contains("hevc_videotoolbox"),
            "missing videotoolbox mention: {}",
            m
        );
    }

    #[test]
    fn no_gpu_message_has_platform_specific_tail() {
        // The tail of the message branches on platform. We can only assert
        // on the branch this build was compiled for — the other branches
        // are validated by readers and CI on each OS.
        let m = no_gpu_message();
        if cfg!(target_os = "macos") {
            assert!(m.contains("brew install ffmpeg"));
            assert!(m.contains("VideoToolbox"));
        } else if running_in_container() {
            // CI normally runs outside a container, so this branch only
            // fires on container-based runners. Keep the assertion lax.
            assert!(m.contains("--device /dev/dri") || m.contains("--gpus all"));
        } else {
            assert!(m.contains("nvidia-smi"));
            assert!(m.contains("vainfo"));
            assert!(m.contains("docs/NAS.md"));
        }
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

    // ── run_output_with_timeout tests ────────────────────────────────────────

    #[test]
    fn test_run_output_with_timeout_fast_command() {
        let mut cmd = Command::new("echo");
        cmd.arg("hello")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = run_output_with_timeout(cmd, Duration::from_secs(5), "echo")
            .expect("echo should succeed");
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("hello"));
    }

    #[test]
    fn test_run_output_with_timeout_fires_on_slow_command() {
        let mut cmd = Command::new("sleep");
        cmd.arg("60").stdout(Stdio::piped()).stderr(Stdio::piped());
        let started = std::time::Instant::now();
        let result = run_output_with_timeout(cmd, Duration::from_millis(200), "sleep");
        let elapsed = started.elapsed();
        assert!(result.is_err(), "expected timeout error, got: {:?}", result);
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("timed out"),
            "error must mention timeout: {err}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "watchdog took {elapsed:?}, expected < 2s"
        );
    }

    #[test]
    fn test_run_output_with_timeout_nonzero_exit() {
        let mut cmd = Command::new("false");
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let output = run_output_with_timeout(cmd, Duration::from_secs(5), "false")
            .expect("false should complete");
        assert!(!output.status.success());
    }
}
