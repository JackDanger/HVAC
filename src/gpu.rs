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

fn nvidia_supports_10bit_hevc(name: &str) -> bool {
    let lower = name.to_lowercase();
    for fragment in NVENC_NO_10BIT_HEVC {
        if lower.contains(&fragment.to_lowercase()) {
            return false;
        }
    }
    true
}

#[derive(Debug)]
enum FfmpegCheck {
    /// Binary has the encoder and runs cleanly.
    Ready,
    /// Binary exits non-zero or crashes (e.g. missing shared library).
    Broken { path: String, error: String },
    /// Binary runs but the encoder is not compiled in.
    EncoderMissing { path: String },
    /// No ffmpeg binary found in PATH.
    NotFound,
}

/// Resolve the first `ffmpeg` on PATH, returning its absolute path.
fn which_ffmpeg() -> String {
    let path_var = std::env::var("PATH").unwrap_or_default();
    for dir in path_var.split(':') {
        let candidate = format!("{}/ffmpeg", dir);
        if std::path::Path::new(&candidate).is_file() {
            return candidate;
        }
    }
    "ffmpeg".to_string()
}

/// Run `binary -hide_banner -encoders` and determine whether `encoder` is compiled in.
/// Wrapped in `run_output_with_timeout` so a broken-driver ffmpeg can't hang
/// the startup probe indefinitely.
fn check_ffmpeg_binary(binary: &str, encoder: &str) -> FfmpegCheck {
    let mut cmd = Command::new(binary);
    cmd.args(["-hide_banner", "-encoders"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match run_output_with_timeout(cmd, Duration::from_secs(10), binary) {
        Err(e) => {
            // Distinguish "binary not on PATH" (NotFound) from "spawned but
            // failed / timed out" (Broken). NotFound is anyhow-wrapped so we
            // walk the source chain.
            let is_not_found = e
                .chain()
                .filter_map(|c| c.downcast_ref::<std::io::Error>())
                .any(|io| io.kind() == std::io::ErrorKind::NotFound);
            if is_not_found {
                FfmpegCheck::NotFound
            } else {
                FfmpegCheck::Broken {
                    path: binary.to_string(),
                    error: e.to_string(),
                }
            }
        }
        Ok(out) if !out.status.success() => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            FfmpegCheck::Broken {
                path: binary.to_string(),
                error: if stderr.is_empty() {
                    format!("exited with status {}", out.status)
                } else {
                    stderr
                },
            }
        }
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if stdout.contains(encoder) {
                FfmpegCheck::Ready
            } else {
                FfmpegCheck::EncoderMissing {
                    path: binary.to_string(),
                }
            }
        }
    }
}

/// Query whether `path` is owned by an installed package.
/// Returns the package name (e.g. `"ffmpeg"`) or `None` for custom builds.
fn owning_package(path: &str) -> Option<String> {
    // Debian/Ubuntu: "package: /path"
    if let Ok(out) = Command::new("dpkg").args(["-S", path]).output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Some(pkg) = s.split(':').next() {
                let pkg = pkg.trim();
                if !pkg.is_empty() {
                    return Some(pkg.to_string());
                }
            }
        }
    }
    // RPM-based (Fedora, RHEL, CentOS)
    if let Ok(out) = Command::new("rpm").args(["-qf", path]).output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() && !s.contains("not owned") {
                return Some(s);
            }
        }
    }
    // Arch Linux
    if let Ok(out) = Command::new("pacman").args(["-Qo", path]).output() {
        if out.status.success() {
            // "path is owned by package version"
            if let Some(pkg) = String::from_utf8_lossy(&out.stdout)
                .split("owned by ")
                .nth(1)
                .and_then(|s| s.split_whitespace().next())
                .map(String::from)
            {
                return Some(pkg);
            }
        }
    }
    None
}

/// Extract the missing `.so` name from a dynamic-linker error, e.g.
/// "error while loading shared libraries: libdav1d.so.6: cannot open..."
/// → `"libdav1d.so.6"`.
fn extract_missing_lib(error: &str) -> Option<String> {
    error
        .split("shared libraries: ")
        .nth(1)?
        .split(':')
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| s.contains(".so"))
}

/// Given a missing library like `"libdav1d.so.6"`, look for a different
/// version of the same library via `ldconfig -p` and return its full path.
fn find_alt_lib(missing: &str) -> Option<String> {
    // "libdav1d.so.6" → base soname prefix "libdav1d.so"
    let dot_so = missing.find(".so")?;
    let base = &missing[..dot_so + 3]; // "libdav1d.so"

    let out = Command::new("ldconfig").arg("-p").output().ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        let t = line.trim();
        // Line format: "libfoo.so.N (libc6,...) => /path/to/libfoo.so.N"
        if t.starts_with(base) && !t.starts_with(missing) {
            if let Some(path) = t.split("=>").nth(1).map(str::trim) {
                return Some(path.to_string());
            }
        }
    }
    None
}

/// Return the apt `install` command for the package that historically
/// shipped `missing_lib`, e.g. `"libdav1d.so.6"` → `apt-get install -y libdav1d6`.
/// Returns `None` on non-Debian systems or if the package isn't in the cache.
fn apt_install_for_lib(missing_lib: &str) -> Option<String> {
    if !std::path::Path::new("/etc/debian_version").exists() {
        return None;
    }
    // "libdav1d.so.6" → "libdav1d6"  ("libfoo.so.N" → "libfooN")
    let dot_so = missing_lib.find(".so.")?;
    let lib_name = &missing_lib[..dot_so]; // "libdav1d"
    let ver = missing_lib[dot_so + 4..].split('.').next()?; // "6"
    let pkg = format!("{lib_name}{ver}"); // "libdav1d6"

    let ok = Command::new("apt-cache")
        .args(["show", &pkg])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok {
        Some(format!("apt-get install -y {pkg}"))
    } else {
        None
    }
}

/// Search well-known paths for a working ffmpeg that supports `encoder`,
/// skipping `skip` (the already-checked broken binary).
/// Returns `(path, package_name_or_none)`.
fn find_working_ffmpeg(encoder: &str, skip: &str) -> Option<(String, Option<String>)> {
    for path in &[
        "/usr/bin/ffmpeg",
        "/usr/local/bin/ffmpeg",
        "/opt/ffmpeg/bin/ffmpeg",
        "/snap/bin/ffmpeg",
    ] {
        if *path == skip || !std::path::Path::new(path).is_file() {
            continue;
        }
        if let FfmpegCheck::Ready = check_ffmpeg_binary(path, encoder) {
            let pkg = owning_package(path);
            return Some((path.to_string(), pkg));
        }
    }
    None
}

/// Return the package-manager reinstall command for the given package name.
fn reinstall_cmd(pkg: &str) -> String {
    if std::path::Path::new("/etc/debian_version").exists() {
        return format!("apt-get install --reinstall {pkg}");
    }
    if std::path::Path::new("/etc/redhat-release").exists()
        || std::path::Path::new("/etc/fedora-release").exists()
    {
        return format!("dnf reinstall {pkg}");
    }
    if std::path::Path::new("/etc/arch-release").exists() {
        return format!("pacman -S {pkg}");
    }
    format!("reinstall {pkg} via your package manager")
}

/// Return a distro-appropriate one-liner to install ffmpeg from scratch.
fn os_ffmpeg_install_cmd() -> &'static str {
    if std::path::Path::new("/etc/debian_version").exists() {
        return "apt-get install -y ffmpeg";
    }
    if std::path::Path::new("/etc/redhat-release").exists()
        || std::path::Path::new("/etc/fedora-release").exists()
    {
        return "dnf install -y ffmpeg";
    }
    if std::path::Path::new("/etc/arch-release").exists() {
        return "pacman -S --noconfirm ffmpeg";
    }
    if cfg!(target_os = "macos") {
        return "brew install ffmpeg";
    }
    "install ffmpeg from your package manager"
}

fn ffmpeg_broken_message(
    gpu_name: &str,
    encoder: &str,
    ffmpeg_path: &str,
    error: &str,
    broken_package: Option<&str>,
    working_alt: Option<(&str, Option<&str>)>,
) -> String {
    // The dynamic linker error looks like:
    //   "/path/ffmpeg: error while loading shared libraries: libfoo.so.N: ..."
    // Strip the binary path prefix and the boilerplate "error while loading..." so
    // we show just the library name and reason: "libfoo.so.N: cannot open...".
    let short_error = error
        .split_once(": ")
        .map(|x| x.1)
        .map(|s| {
            s.strip_prefix("error while loading shared libraries: ")
                .unwrap_or(s)
        })
        .filter(|s| !s.is_empty())
        .unwrap_or(error);

    let broken_label = match broken_package {
        Some(pkg) => format!("(package '{pkg}')"),
        None => {
            let size = file_size_label(ffmpeg_path);
            format!("(custom build{size} — not installed by any package manager)")
        }
    };

    let mut msg = format!(
        "GPU found ({gpu_name}) — {ffmpeg_path} cannot start.\n\n  \
         {ffmpeg_path} {broken_label}\n  \
         failed to load: {short_error}\n"
    );

    // For a package-managed binary, reinstall is the right fix.
    if let Some(pkg) = broken_package {
        msg.push_str(&format!("\n  To fix:\n    {}\n", reinstall_cmd(pkg)));
        return msg;
    }

    // Custom build. Explain the root cause when we can identify it.
    // Do NOT suggest cross-soname symlinks — soname bumps signal ABI changes;
    // symlinking across them causes silent runtime corruption (e.g. dav1d
    // struct layout changed between .so.6 and .so.7).
    if let Some(missing) = extract_missing_lib(error) {
        if let Some(alt_lib) = find_alt_lib(&missing) {
            msg.push_str(&format!(
                "\n  The system has {alt_lib} but not {missing}.\n  \
                 The soname was bumped because of an ABI change — symlinking\n  \
                 across versions is unsafe and can silently corrupt media decoding.\n  \
                 The custom build needs to be rebuilt against current libraries, or removed.\n"
            ));
        } else if let Some(apt_cmd) = apt_install_for_lib(&missing) {
            // Library entirely absent — installing it may restore the custom build.
            msg.push_str(&format!(
                "\n  {missing} is not installed. To install it:\n    {apt_cmd}\n"
            ));
        }
    }

    match working_alt {
        Some((alt_path, alt_pkg)) => {
            // A working ffmpeg with the required encoder exists. Removing the
            // broken custom build is the correct action — it already covers
            // hvac's needs and was independently validated above.
            let pkg_note = match alt_pkg {
                Some(pkg) => format!(" (package '{pkg}')"),
                None => String::new(),
            };
            msg.push_str(&format!(
                "\n  The system ffmpeg{pkg_note} at {alt_path} has {encoder}\n  \
                 and everything else hvac needs.\n\n  \
                 Recommended fix — remove the broken custom build:\n    \
                 rm {ffmpeg_path}\n\n  \
                 If this build existed for a specific reason, back it up first:\n    \
                 mv {ffmpeg_path} {ffmpeg_path}.broken\n"
            ));
        }
        None => {
            msg.push_str(&format!(
                "\n  No other ffmpeg with {encoder} found. To install one:\n    {}\n",
                os_ffmpeg_install_cmd()
            ));
        }
    }

    msg
}

/// Return a human-readable size string for a file, e.g. "24 MB".
fn file_size_label(path: &str) -> String {
    std::fs::metadata(path)
        .map(|m| {
            let bytes = m.len();
            if bytes >= 1024 * 1024 {
                format!(", {} MB", bytes / (1024 * 1024))
            } else if bytes >= 1024 {
                format!(", {} KB", bytes / 1024)
            } else {
                String::new()
            }
        })
        .unwrap_or_default()
}

fn ffmpeg_encoder_missing_message(
    gpu_name: &str,
    encoder: &str,
    ffmpeg_path: &str,
    broken_package: Option<&str>,
    working_alt: Option<(&str, Option<&str>)>,
) -> String {
    let broken_label = match broken_package {
        Some(pkg) => format!("(package '{pkg}') "),
        None => "(custom build) ".to_string(),
    };
    let mut msg = format!(
        "GPU found ({gpu_name}) — the default ffmpeg does not have {encoder}.\n\n  \
         {ffmpeg_path} {broken_label}was not built with {encoder}.\n"
    );
    match (working_alt, broken_package) {
        (Some((alt_path, alt_pkg)), _) => {
            let alt_label = match alt_pkg {
                Some(pkg) => format!("(package '{pkg}') "),
                None => String::new(),
            };
            msg.push_str(&format!(
                "\n  A working ffmpeg {alt_label}at {alt_path} has {encoder} support,\n  \
                 but it is shadowed by {ffmpeg_path}.\n"
            ));
            match broken_package {
                Some(pkg) => msg.push_str(&format!("\n  To fix:\n    {}\n", reinstall_cmd(pkg))),
                None => msg.push_str(&format!(
                    "\n  To fix, remove the custom build:\n    rm {ffmpeg_path}\n"
                )),
            }
        }
        (None, Some(pkg)) => {
            msg.push_str(&format!(
                "\n  This package may not include GPU encoders. To install one that does:\n    {}\n",
                reinstall_cmd(pkg)
            ));
        }
        (None, None) => {
            msg.push_str(&format!(
                "\n  To install ffmpeg with {encoder} support:\n    {}\n",
                os_ffmpeg_install_cmd()
            ));
        }
    }
    msg
}

fn ffmpeg_not_found_message(gpu_name: &str, encoder: &str) -> String {
    format!(
        "GPU found ({gpu_name}) — ffmpeg not found in PATH.\n\n  \
         To install ffmpeg with {encoder} support:\n    {}\n",
        os_ffmpeg_install_cmd()
    )
}

/// Detect available GPU for h265 encoding.
/// Checks NVIDIA first (hevc_nvenc), then Intel (hevc_vaapi), then Apple (hevc_videotoolbox).
/// When GPU hardware is found but ffmpeg can't use it, bails with a specific diagnosis.
pub fn detect_gpu() -> Result<GpuInfo> {
    let ffmpeg = which_ffmpeg();

    // ── NVIDIA ────────────────────────────────────────────────────────────────
    if let Ok(nvidia_name) = detect_nvidia() {
        match check_ffmpeg_binary(&ffmpeg, "hevc_nvenc") {
            FfmpegCheck::Ready => {
                let supports_10bit = nvidia_supports_10bit_hevc(&nvidia_name);
                return Ok(GpuInfo {
                    name: nvidia_name,
                    encoder: "hevc_nvenc".to_string(),
                    kind: GpuKind::Nvidia,
                    supports_10bit_hevc: supports_10bit,
                });
            }
            FfmpegCheck::Broken { path, error } => {
                let broken_pkg = owning_package(&path);
                let alt = find_working_ffmpeg("hevc_nvenc", &path);
                bail!(
                    "{}",
                    ffmpeg_broken_message(
                        &nvidia_name,
                        "hevc_nvenc",
                        &path,
                        &error,
                        broken_pkg.as_deref(),
                        alt.as_ref().map(|(p, pkg)| (p.as_str(), pkg.as_deref())),
                    )
                );
            }
            FfmpegCheck::EncoderMissing { path } => {
                let broken_pkg = owning_package(&path);
                let alt = find_working_ffmpeg("hevc_nvenc", &path);
                bail!(
                    "{}",
                    ffmpeg_encoder_missing_message(
                        &nvidia_name,
                        "hevc_nvenc",
                        &path,
                        broken_pkg.as_deref(),
                        alt.as_ref().map(|(p, pkg)| (p.as_str(), pkg.as_deref())),
                    )
                );
            }
            FfmpegCheck::NotFound => {
                bail!("{}", ffmpeg_not_found_message(&nvidia_name, "hevc_nvenc"));
            }
        }
    }

    // ── Intel ─────────────────────────────────────────────────────────────────
    if detect_intel_gpu() {
        match check_ffmpeg_binary(&ffmpeg, "hevc_vaapi") {
            FfmpegCheck::Ready => {
                return Ok(GpuInfo {
                    name: "Intel GPU (VAAPI)".to_string(),
                    encoder: "hevc_vaapi".to_string(),
                    kind: GpuKind::Intel,
                    supports_10bit_hevc: true,
                });
            }
            FfmpegCheck::Broken { path, error } => {
                let broken_pkg = owning_package(&path);
                let alt = find_working_ffmpeg("hevc_vaapi", &path);
                bail!(
                    "{}",
                    ffmpeg_broken_message(
                        "Intel GPU",
                        "hevc_vaapi",
                        &path,
                        &error,
                        broken_pkg.as_deref(),
                        alt.as_ref().map(|(p, pkg)| (p.as_str(), pkg.as_deref())),
                    )
                );
            }
            FfmpegCheck::EncoderMissing { path } => {
                let broken_pkg = owning_package(&path);
                let alt = find_working_ffmpeg("hevc_vaapi", &path);
                bail!(
                    "{}",
                    ffmpeg_encoder_missing_message(
                        "Intel GPU",
                        "hevc_vaapi",
                        &path,
                        broken_pkg.as_deref(),
                        alt.as_ref().map(|(p, pkg)| (p.as_str(), pkg.as_deref())),
                    )
                );
            }
            FfmpegCheck::NotFound => {
                bail!("{}", ffmpeg_not_found_message("Intel GPU", "hevc_vaapi"));
            }
        }
    }

    // ── Apple VideoToolbox ────────────────────────────────────────────────────
    if detect_apple_gpu() {
        match check_ffmpeg_binary(&ffmpeg, "hevc_videotoolbox") {
            FfmpegCheck::Ready => {
                return Ok(GpuInfo {
                    name: detect_apple_chip_name(),
                    encoder: "hevc_videotoolbox".to_string(),
                    kind: GpuKind::Apple,
                    supports_10bit_hevc: true,
                });
            }
            FfmpegCheck::Broken { path, error } => {
                let broken_pkg = owning_package(&path);
                let alt = find_working_ffmpeg("hevc_videotoolbox", &path);
                bail!(
                    "{}",
                    ffmpeg_broken_message(
                        "Apple GPU",
                        "hevc_videotoolbox",
                        &path,
                        &error,
                        broken_pkg.as_deref(),
                        alt.as_ref().map(|(p, pkg)| (p.as_str(), pkg.as_deref())),
                    )
                );
            }
            FfmpegCheck::EncoderMissing { path } => {
                let broken_pkg = owning_package(&path);
                let alt = find_working_ffmpeg("hevc_videotoolbox", &path);
                bail!(
                    "{}",
                    ffmpeg_encoder_missing_message(
                        "Apple GPU",
                        "hevc_videotoolbox",
                        &path,
                        broken_pkg.as_deref(),
                        alt.as_ref().map(|(p, pkg)| (p.as_str(), pkg.as_deref())),
                    )
                );
            }
            FfmpegCheck::NotFound => {
                bail!(
                    "{}",
                    ffmpeg_not_found_message("Apple GPU", "hevc_videotoolbox")
                );
            }
        }
    }

    bail!("{}", no_gpu_message())
}

fn no_gpu_message() -> String {
    let mut msg = String::from(
        "No GPU found for h265 encoding!\n\
         hvac requires one of:\n\
         - NVIDIA GPU with NVENC support (hevc_nvenc)\n\
         - Intel GPU with VAAPI support (hevc_vaapi)\n\
         - Apple Silicon or Mac with VideoToolbox (hevc_videotoolbox)\n\n",
    );

    if cfg!(target_os = "macos") {
        msg.push_str(
            "On macOS, VideoToolbox ships with the OS. This message usually means:\n\
             - ffmpeg is not installed:  brew install ffmpeg\n\
             - or a non-default ffmpeg build lacks hevc_videotoolbox; reinstall from Homebrew\n",
        );
    } else if running_in_container() {
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

fn running_in_container() -> bool {
    if std::path::Path::new("/.dockerenv").exists()
        || std::path::Path::new("/run/.containerenv").exists()
    {
        return true;
    }
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

#[cfg(test)]
fn has_ffmpeg_encoder(encoder: &str) -> bool {
    let ffmpeg = which_ffmpeg();
    matches!(check_ffmpeg_binary(&ffmpeg, encoder), FfmpegCheck::Ready)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_has_ffmpeg_encoder() {
        // Check the PATH-resolved ffmpeg first; fall back to searching well-known
        // system paths in case the PATH binary is broken (e.g. missing .so).
        let encoders = ["libx265", "hevc_nvenc", "hevc_vaapi", "hevc_videotoolbox"];
        let via_path = encoders.iter().any(|e| has_ffmpeg_encoder(e));
        // find_working_ffmpeg with a dummy skip path searches all known locations.
        let via_system = encoders
            .iter()
            .any(|e| find_working_ffmpeg(e, "/nonexistent").is_some());
        assert!(
            via_path || via_system,
            "no working ffmpeg with an h265 encoder found on this system"
        );
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
        assert!(!nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 750"));
        assert!(!nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 750 Ti"));
    }

    #[test]
    fn test_10bit_unsupported_maxwell_second_gen() {
        assert!(!nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 950"));
        assert!(!nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 960"));
        assert!(!nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 970"));
        assert!(!nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 980"));
        assert!(!nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 980 Ti"));
    }

    #[test]
    fn test_10bit_unsupported_early_pascal() {
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
        assert!(nvidia_supports_10bit_hevc("NVIDIA GeForce RTX 2060"));
        assert!(nvidia_supports_10bit_hevc("NVIDIA GeForce RTX 2080 Ti"));
        assert!(nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 1660 Ti"));
        assert!(nvidia_supports_10bit_hevc("NVIDIA GeForce GTX 1650"));
        assert!(nvidia_supports_10bit_hevc("NVIDIA GeForce RTX 3060"));
        assert!(nvidia_supports_10bit_hevc("NVIDIA GeForce RTX 3090"));
        assert!(nvidia_supports_10bit_hevc("NVIDIA GeForce RTX 4090"));
        assert!(nvidia_supports_10bit_hevc("NVIDIA A100-SXM4-80GB"));
        assert!(nvidia_supports_10bit_hevc("Quadro RTX 5000"));
        assert!(nvidia_supports_10bit_hevc("Tesla T4"));
    }

    #[test]
    fn test_10bit_case_insensitive() {
        assert!(!nvidia_supports_10bit_hevc("nvidia geforce gtx 970"));
        assert!(!nvidia_supports_10bit_hevc("GEFORCE GTX 1080"));
    }

    #[test]
    fn no_gpu_message_mentions_all_three_encoders() {
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
        let m = no_gpu_message();
        if cfg!(target_os = "macos") {
            assert!(m.contains("brew install ffmpeg"));
            assert!(m.contains("VideoToolbox"));
        } else if running_in_container() {
            assert!(m.contains("--device /dev/dri") || m.contains("--gpus all"));
        } else {
            assert!(m.contains("nvidia-smi"));
            assert!(m.contains("vainfo"));
            assert!(m.contains("docs/NAS.md"));
        }
    }

    #[test]
    fn test_detect_gpu_returns_result() {
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

    // Linux-only: the ABI-change message branch fires only when find_alt_lib
    // returns Some, which requires ldconfig (Linux). On macOS the function
    // returns None and the ABI explanation is correctly omitted.
    #[cfg(target_os = "linux")]
    #[test]
    fn ffmpeg_broken_custom_with_working_alt_recommends_rm() {
        // When a working system ffmpeg has the encoder and the custom build is broken,
        // removing the custom build is the correct recommendation, not a cross-soname symlink.
        let msg = ffmpeg_broken_message(
            "NVIDIA GeForce RTX 2060",
            "hevc_nvenc",
            "/usr/local/bin/ffmpeg",
            "/usr/local/bin/ffmpeg: error while loading shared libraries: libdav1d.so.6: cannot open shared object file: No such file or directory",
            None, // custom build, not package-managed
            Some(("/usr/bin/ffmpeg", Some("ffmpeg"))),
        );
        assert!(msg.contains("NVIDIA GeForce RTX 2060"));
        assert!(msg.contains("libdav1d.so.6"));
        assert!(msg.contains("custom build"));
        assert!(msg.contains("/usr/bin/ffmpeg"));
        // Must recommend rm — the system package covers hvac's needs.
        assert!(
            msg.contains("rm /usr/local/bin/ffmpeg"),
            "should recommend rm when system ffmpeg works"
        );
        // Must offer mv as a zero-risk backup option.
        assert!(
            msg.contains("mv /usr/local/bin/ffmpeg"),
            "should offer mv as backup option"
        );
        // Must NOT suggest a cross-soname symlink.
        assert!(
            !msg.contains("ln -s"),
            "must not suggest cross-soname symlink"
        );
        // Must explain why a symlink is wrong.
        assert!(
            msg.contains("ABI"),
            "should explain that soname bump = ABI change"
        );
    }

    #[test]
    fn ffmpeg_broken_pkg_reinstall() {
        // Package-managed ffmpeg is broken — suggest reinstall only, no library digging.
        let msg = ffmpeg_broken_message(
            "Intel GPU",
            "hevc_vaapi",
            "/usr/bin/ffmpeg",
            "exited with status 127",
            Some("ffmpeg"), // broken binary IS the package
            None,
        );
        assert!(msg.contains("Intel GPU"));
        assert!(msg.contains("reinstall"));
        assert!(
            !msg.contains("rm /usr/bin/ffmpeg"),
            "should not suggest rm for a package binary"
        );
    }

    #[test]
    fn ffmpeg_broken_custom_no_alt_no_lib_error() {
        // Custom build broken with a non-library error and no fallback.
        let msg = ffmpeg_broken_message(
            "Intel GPU",
            "hevc_vaapi",
            "/usr/local/bin/ffmpeg",
            "exited with status 127",
            None,
            None,
        );
        assert!(msg.contains("Intel GPU"));
        assert!(msg.contains("install"));
        // No rm suggestion for custom build without clear library fix.
        assert!(!msg.contains("rm /usr/local/bin/ffmpeg"));
    }

    #[test]
    fn ffmpeg_encoder_missing_shows_install_cmd() {
        let msg = ffmpeg_encoder_missing_message(
            "NVIDIA GeForce RTX 2060",
            "hevc_nvenc",
            "/usr/bin/ffmpeg",
            None,
            None,
        );
        assert!(msg.contains("hevc_nvenc"));
        assert!(msg.contains("install"));
    }
}
