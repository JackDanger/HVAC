mod config;
mod gpu;
mod iso;
mod probe;
mod scanner;
mod transcode;
mod util;

use anyhow::{Context, Result};
use clap::Parser;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use util::format_size;

const MAX_SESSION_RETRIES: u32 = 5;

/// Skip files shorter than this duration (seconds).  Animated GIFs, single-frame
/// WebP-as-mp4 stubs, and similar artefacts pass the extension scan but produce
/// unusable output (or fail validation) when transcoded — skip them with a clear
/// message instead.  Override per-run with `--min-duration <SECS>`.
const MIN_TRANSCODE_DURATION_SECS: f64 = 1.0;

/// Returns true when `duration_secs` is a valid, positive measurement that is
/// strictly below `min_duration_secs` — i.e. the file is short enough to skip.
/// A duration of `0.0` (or negative) means ffprobe could not determine the
/// duration; those files fall through to the normal codepath rather than being
/// silently skipped here.
fn is_too_short(duration_secs: f64, min_duration_secs: f64) -> bool {
    duration_secs > 0.0 && duration_secs < min_duration_secs
}

/// After this many cumulative NVENC session-limit hits across the entire run,
/// permanently freeze `max_encoders` at the lowest observed working value and
/// disable auto-ramping. This stops the climb-fail-retry-climb cycle on
/// consumer GeForce cards (3-session NVENC cap).
const MAX_SESSION_LIMIT_BEFORE_FREEZE: u32 = 3;

/// Returns true once the cumulative session-limit hit count reaches the
/// freeze threshold. `hits` is the post-increment count.
fn should_freeze(hits: u32) -> bool {
    hits >= MAX_SESSION_LIMIT_BEFORE_FREEZE
}

/// Display symbols — ASCII fallbacks when locale doesn't support UTF-8.
struct Symbols {
    ellipsis: &'static str,
    bar_filled: &'static str,
    bar_head: &'static str,
    bar_empty: &'static str,
    hourglass: &'static str,
    play: &'static str,
    check: &'static str,
    cross: &'static str,
    arrow: &'static str,
}

const UNICODE_SYMBOLS: Symbols = Symbols {
    ellipsis: "\u{2026}",
    bar_filled: "\u{2501}",
    bar_head: "\u{2578}",
    bar_empty: "\u{2500}",
    hourglass: "\u{23f3}",
    play: "\u{25b6}",
    check: "\u{2713}",
    cross: "\u{2717}",
    arrow: "\u{2192}",
};

const ASCII_SYMBOLS: Symbols = Symbols {
    ellipsis: "..",
    bar_filled: "=",
    bar_head: ">",
    bar_empty: "-",
    hourglass: "~",
    play: ">",
    check: "+",
    cross: "x",
    arrow: "->",
};

/// Return the number of columns in the terminal attached to stderr.
/// Falls back to 80 if unavailable (no tty, piped output, etc.).
fn terminal_cols() -> usize {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            ws.ws_col as usize
        } else {
            80
        }
    }
}

/// Overhead (terminal columns) consumed by the widest rendered line format,
/// excluding the file name.  This is the disk-wait format:
///   "  ~           <name> (1000.0GB)  waiting for disk"
///    2+1+11              +2+8+1      +18  = 43, plus 1 margin = 44
const LINE_FORMAT_OVERHEAD: usize = 44;

/// Maximum file-name display width for a terminal of `cols` columns.
/// Keeps every rendered line within `cols` even for worst-case sizes.
fn max_name_for_cols(cols: usize) -> usize {
    cols.saturating_sub(LINE_FORMAT_OVERHEAD).max(20)
}

/// Compute the source path used as the output-file name stem.
///
/// This logic is shared between the pre-transcode resume check and the worker
/// thread.  Both must produce identical results; if they diverge the resume
/// check looks for the wrong output file and re-transcodes on every run.
///
///   any ISO (single or multi-file) → ISO path  (Movie Title.iso → Movie Title.transcoded.mkv)
///   regular file                   → file path itself
///
/// Inner track names (e.g. "00000.M2TS", "VTS_01_1.VOB") are meaningless
/// technical identifiers; the ISO itself always carries the meaningful title.
fn output_stem_for_item(
    file: &std::path::Path,
    _inner_path: Option<&str>,
    _inner_paths: Option<&[String]>,
) -> std::path::PathBuf {
    file.to_path_buf()
}

/// Probe whether the parent directory of `source` allows file creation.
///
/// In `--overwrite` mode hvac writes a `.hvac_tmp_*` file alongside the source
/// and `fs::rename`s it over the original; if the parent directory is
/// read-only (mounted ro, ACL deny, etc.) that rename fails *after* a
/// multi-minute encode has already burned GPU time.  This helper performs the
/// same kind of write up front so we can fail-fast with a clear message.
///
/// Creates and immediately removes a uniquely-named probe file.  Returns
/// `false` if the source has no parent or the create fails for any reason.
// Convenience wrapper around `dir_is_writable` that resolves the source's
// parent. The hot path uses `dir_is_writable_cached` directly so this is
// only exercised by the unit tests today; kept for callers that probe one
// source at a time without needing the dir cache.
#[cfg_attr(not(test), allow(dead_code))]
fn parent_is_writable(source: &Path) -> bool {
    let parent = match source.parent() {
        Some(p) => p,
        None => return false,
    };
    dir_is_writable(parent)
}

/// Probe whether `dir` allows file creation by attempting to create (and
/// immediately remove) a uniquely-named file inside it.
///
/// Two failure modes the naive `File::create(<pid-only path>)` version
/// glossed over:
///   - PID isn't unique enough. Two concurrent runs (same host, same
///     mounted media tree) collide on `.hvac_writable_check_<pid>` if the
///     OS happens to recycle a PID, and `File::create` would *truncate*
///     a user-owned file with that exact name. Use `create_new(true)`
///     plus a nanosecond-resolution timestamp so collisions never silently
///     win.
///   - Cleanup failure mustn't be ignored: returning `true` after a
///     failed `remove_file` would leave stray probe files in the user's
///     media directory. Treat that as a probe failure and let the caller
///     surface the directory as non-writable.
fn dir_is_writable(dir: &Path) -> bool {
    use std::fs::OpenOptions;
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let probe = dir.join(format!(
        ".hvac_writable_check_{}_{}",
        std::process::id(),
        nanos
    ));

    let create_result = OpenOptions::new().write(true).create_new(true).open(&probe);
    if create_result.is_err() {
        return false;
    }

    match std::fs::remove_file(&probe) {
        Ok(()) => true,
        Err(e) => {
            // We could create but not delete — likely an aggressive ACL
            // (e.g. SMB share with create-but-not-delete rights) or a
            // transient lock. Surface the dir as non-writable so the
            // user gets a pre-flight skip rather than a stray probe file
            // in their media tree.
            log::warn!(
                "dir_is_writable: probe at {:?} created but not removed: {} \
                 — treating directory as non-writable",
                probe,
                e
            );
            false
        }
    }
}

/// Cached writable-check: avoid hammering the filesystem when 1000s of files
/// share the same parent directory.
fn dir_is_writable_cached(cache: &mut HashMap<PathBuf, bool>, dir: &Path) -> bool {
    if let Some(&v) = cache.get(dir) {
        return v;
    }
    let v = dir_is_writable(dir);
    cache.insert(dir.to_path_buf(), v);
    v
}

fn detect_symbols() -> &'static Symbols {
    // Check the actual system locale, not just env vars.
    // LANG=en_US.UTF-8 can be set even when the locale isn't installed,
    // causing the C library to fall back to ASCII.
    unsafe {
        // Initialize locale from environment (required before nl_langinfo)
        libc::setlocale(libc::LC_ALL, c"".as_ptr());
        let codeset = libc::nl_langinfo(libc::CODESET);
        if !codeset.is_null() {
            let cs = std::ffi::CStr::from_ptr(codeset)
                .to_string_lossy()
                .to_lowercase();
            if cs.contains("utf-8") || cs.contains("utf8") {
                return &UNICODE_SYMBOLS;
            }
        }
    }
    &ASCII_SYMBOLS
}

static CANCELLED: AtomicBool = AtomicBool::new(false);

/// Directories where tmp files may exist — populated before encoding starts,
/// read by the CTRL-C handler for cleanup on force-quit.
static TMP_DIRS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

#[derive(Parser, Debug)]
#[command(name = "hvac", version, about = "GPU-accelerated media transcoder")]
struct Cli {
    /// Directory to scan for media files
    #[arg(required_unless_present = "dump_config")]
    path: Option<PathBuf>,

    /// Path to YAML config file (uses built-in defaults if omitted)
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Print the built-in default config to stdout and exit
    #[arg(long)]
    dump_config: bool,

    /// Suppress the banner shown when running with built-in default config
    #[arg(short, long)]
    quiet: bool,

    /// Keep originals: write `.transcoded.<ext>` copies alongside instead of overwriting in place
    #[arg(long, default_value_t = false)]
    no_overwrite: bool,

    /// Dry run — print what would be transcoded and exit without touching anything
    #[arg(long, default_value_t = false)]
    dry_run: bool,

    /// Number of parallel encode jobs (auto-detected from GPU if not set)
    #[arg(short, long)]
    jobs: Option<usize>,

    /// Output directory for transcoded files (overrides config)
    #[arg(short, long)]
    output_dir: Option<PathBuf>,

    /// Replace originals with transcoded copies after all encodes complete
    #[arg(long, default_value_t = false)]
    replace: bool,

    /// Skip files shorter than this many seconds (animated GIFs, single-frame stubs)
    #[arg(long, default_value_t = MIN_TRANSCODE_DURATION_SECS)]
    min_duration: f64,

    /// Maximum seconds ffprobe may run on a single file before being killed.
    /// Protects against hangs caused by stale NFS / unresponsive SMB mounts.
    #[arg(long, default_value_t = 30)]
    probe_timeout: u64,
}

/// File ready to transcode, with pre-probed metadata.
struct WorkItem {
    path: PathBuf,
    bitrate_kbps: u32,
    duration_secs: f64,
    pix_fmt: String,
    source_size: u64,
    /// For ISO entries: path to the ISO file.
    iso_path: Option<PathBuf>,
    /// For ISO with single file: path inside the ISO.
    inner_path: Option<String>,
    /// For ISO main feature with multiple files: paths to concatenate in order.
    inner_paths: Option<Vec<String>>,
}

/// Combine the representative probe (the first inner file of an ISO main
/// feature) with optional per-file probes of the remaining inner files into
/// `(bitrate_kbps, duration_secs)` for the WorkItem.
///
/// - `bitrate_kbps` is the maximum across all successful probes. Using max
///   ensures `-maxrate` doesn't throttle later, higher-bitrate VOBs.
/// - `duration_secs` is the sum of every probe's duration. If any per-file
///   probe failed (None) we fall back to `representative.duration_secs *
///   total_file_count` since the sum would otherwise undercount.
/// - `total_file_count` should be the total number of inner files in the
///   feature (including the representative). 0 or 1 means "not multi-file"
///   and we just return the representative's values unchanged.
fn aggregate_iso_probes(
    representative: &probe::MediaInfo,
    extra_probes: &[Option<probe::MediaInfo>],
    total_file_count: usize,
) -> (u32, f64) {
    if total_file_count <= 1 {
        return (representative.bitrate_kbps, representative.duration_secs);
    }

    let mut max_bitrate = representative.bitrate_kbps;
    let mut sum_duration = representative.duration_secs;
    let mut all_succeeded = true;

    for p in extra_probes {
        match p {
            Some(info) => {
                if info.bitrate_kbps > max_bitrate {
                    max_bitrate = info.bitrate_kbps;
                }
                sum_duration += info.duration_secs;
            }
            None => all_succeeded = false,
        }
    }

    let duration_secs = if all_succeeded {
        sum_duration
    } else {
        representative.duration_secs * total_file_count as f64
    };

    (max_bitrate, duration_secs)
}

/// Per-worker display slot for the render thread.
struct WorkerSlot {
    info: Mutex<Option<(String, String)>>,
    progress: AtomicU64,
    speed: AtomicU64,
    queued: AtomicBool,
    disk_wait: AtomicBool,
}

fn truncate_name(name: &str, max_len: usize, sym: &Symbols) -> String {
    if name.chars().count() <= max_len {
        name.to_string()
    } else {
        let truncated: String = name.chars().take(max_len - 1).collect();
        format!("{truncated}{}", sym.ellipsis)
    }
}

fn progress_bar_str(fraction: f64, width: usize, sym: &Symbols) -> String {
    let filled = (fraction * width as f64) as usize;
    if filled >= width {
        sym.bar_filled.repeat(width)
    } else {
        format!(
            "{}{}{}",
            sym.bar_filled.repeat(filled),
            sym.bar_head,
            sym.bar_empty.repeat(width.saturating_sub(filled + 1))
        )
    }
}

/// Try to acquire an encoding slot. Returns true if acquired.
/// Uses compare_exchange to atomically check active < max and increment.
fn try_acquire(active_encoders: &AtomicU32, max_encoders: &AtomicU32) -> bool {
    loop {
        let active = active_encoders.load(Ordering::SeqCst);
        let max = max_encoders.load(Ordering::SeqCst);
        if active >= max {
            return false;
        }
        if active_encoders
            .compare_exchange_weak(active, active + 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return true;
        }
    }
}

/// Lower max_encoders to at most new_max.
fn lower_max(max_encoders: &AtomicU32, new_max: u32) {
    loop {
        let current = max_encoders.load(Ordering::SeqCst);
        if current <= new_max {
            break;
        }
        if max_encoders
            .compare_exchange_weak(current, new_max, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            break;
        }
    }
}

/// Returns the banner printed when the built-in default config is used.
/// `use_unicode` selects box-drawing chars vs plain ASCII borders.
fn embedded_config_banner(use_unicode: bool) -> String {
    const W: usize = 62; // inner width (chars between the border columns)

    let content: &[&str] = &[
        "  hvac: using built-in default configuration",
        "",
        "  To customise encoding settings, save the defaults to a file:",
        "",
        "    hvac --dump-config > config.yaml",
        "    $EDITOR config.yaml",
        "    hvac --config config.yaml /path/to/media",
        "",
        "  Suppress this message: hvac --quiet ...",
    ];

    let (tl, tr, bl, br, h, v) = if use_unicode {
        ("╭", "╮", "╰", "╯", "─", "│")
    } else {
        ("+", "+", "+", "+", "-", "|")
    };

    let top = format!("{}{}{}", tl, h.repeat(W), tr);
    let bot = format!("{}{}{}", bl, h.repeat(W), br);

    let mut out = top;
    for line in content {
        let pad = W.saturating_sub(line.chars().count());
        out.push('\n');
        out.push_str(&format!("{}{}{}{}", v, line, " ".repeat(pad), v));
    }
    out.push('\n');
    out.push_str(&bot);
    out
}

fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    // --dump-config: print embedded defaults to stdout and exit (no GPU or path needed)
    if cli.dump_config {
        print!("{}", config::EMBEDDED);
        return Ok(());
    }

    // Safety: clap enforces `path` is present unless --dump-config is set
    let path = cli.path.as_deref().unwrap();

    // Overwriting originals in place is the default; --no-overwrite opts out.
    let overwrite = !cli.no_overwrite;

    // Register CTRL-C handler: first press cancels gracefully, second force-exits
    ctrlc::set_handler(move || {
        // Reset attributes and move to a fresh line
        eprint!("\x1b[0m\r\n");
        if CANCELLED.load(Ordering::Relaxed) {
            cleanup_tmp_dirs();
            std::process::exit(130);
        }
        CANCELLED.store(true, Ordering::Relaxed);
        eprintln!("Cancelling after current encodes finish... (Ctrl-C again to force quit)");
    })
    .ok();

    let sym = detect_symbols();
    let use_unicode = std::ptr::eq(sym, &UNICODE_SYMBOLS);

    // Worker threads write transcoded paths to stdout so callers can pipe them.
    // When stdout is a terminal those writes share the same PTY as the render
    // thread's stderr escape sequences; a println! landing between cursor-up
    // and \x1b[J moves the cursor to the wrong row and the erase misses the
    // old viewport line, leaving a ghost progress line in the scrollback.
    // Suppress stdout output when it is a terminal — the completed-file line
    // on stderr already shows everything a human needs.
    let stdout_is_pipe = unsafe { libc::isatty(libc::STDOUT_FILENO) == 0 };

    // Maximum file name display width: reserve enough columns for the widest line
    // format (disk-wait: ~42 chars overhead) so lines never wrap and cursor-up
    // repositioning stays accurate.
    let max_name: usize = max_name_for_cols(terminal_cols());

    // Load config: explicit --config path must exist; omitting it uses embedded defaults.
    let cfg = match &cli.config {
        Some(p) => config::Config::load(p)
            .with_context(|| format!("Failed to load config from {:?}", p))?,
        None => {
            if !cli.quiet {
                eprintln!("{}", embedded_config_banner(use_unicode));
            }
            config::Config::from_embedded()
        }
    };

    let gpu = gpu::detect_gpu()?;
    eprintln!("GPU: {} ({})", gpu.name, gpu.encoder);

    if let Ok(avail) = util::available_disk_space(path) {
        eprintln!("Disk: {} available", format_size(avail));
    }

    // Warn loudly if we're scanning a network mount: those are the most
    // common cause of indefinite hangs in std::fs::metadata / read_dir
    // (which the directory walker uses with no timeout). The ffprobe
    // watchdog catches probe-phase hangs but not walk-phase hangs.
    if let Some(fs_type) = scanner::detect_network_mount(path) {
        log::warn!(
            "{:?} is on a {} mount; if this filesystem is unresponsive, \
             the directory walk (std::fs::metadata) can hang indefinitely. \
             ffprobe is bounded by --probe-timeout ({}s) but the scan-walk \
             phase is not.",
            path,
            fs_type,
            cli.probe_timeout
        );
    }

    let files = scanner::scan(path, &cfg.media_extensions)?;

    if files.is_empty() {
        eprintln!("No media files found in {:?}", path);
        return Ok(());
    }

    // --- Phase 1: Expand disc images into flat work list ---
    // Each entry is (path, optional iso_path, optional inner_path, optional inner_paths)
    type ExpandedItem = (
        PathBuf,
        Option<PathBuf>,
        Option<String>,
        Option<Vec<String>>,
    );
    let mut expanded: Vec<ExpandedItem> = Vec::new();
    let mut errors = 0u32;

    for file in &files {
        if iso::is_disc_image(file) {
            let iso_name = file.file_name().unwrap_or_default().to_string_lossy();

            let analysis = match iso::analyze_disc(file) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("  skip: {}: {}", iso_name, e);
                    errors += 1;
                    continue;
                }
            };

            if analysis.main_feature.is_empty() {
                eprintln!("  skip: {}: no media files inside", iso_name);
                continue;
            }

            eprintln!(
                "  iso: {} ({:?}, {} main feature files, {} extras)",
                iso_name,
                analysis.disc_type,
                analysis.main_feature.len(),
                analysis.extras.len(),
            );

            if analysis.main_feature.len() == 1 {
                // Single file: use inner_path
                expanded.push((
                    file.clone(),
                    Some(file.clone()),
                    Some(analysis.main_feature[0].path.clone()),
                    None,
                ));
            } else {
                // Multiple files: use inner_paths for concatenation
                let paths: Vec<String> = analysis
                    .main_feature
                    .iter()
                    .map(|f| f.path.clone())
                    .collect();
                // Use the first file's path as the representative for probing
                expanded.push((
                    file.clone(),
                    Some(file.clone()),
                    Some(paths[0].clone()),
                    Some(paths),
                ));
            }
            continue;
        }

        expanded.push((file.clone(), None, None, None));
    }

    // --- Phase 2: Probe all files to partition skip vs. transcode ---
    eprintln!("Scanning {} files...", expanded.len());
    let mut to_transcode: Vec<WorkItem> = Vec::new();
    let mut skipped = 0u32;
    let mut resumed = 0u32;
    // Cache of dir → writable for the pre-flight check.  A typical media
    // library packs thousands of files into a handful of directories, so a
    // per-file probe would re-create the same .hvac_writable_check_<pid> file
    // millions of times.
    let mut writable_cache: HashMap<PathBuf, bool> = HashMap::new();

    let probe_timeout = std::time::Duration::from_secs(cli.probe_timeout);

    for (file, iso_p, inner_p, inner_ps) in &expanded {
        let probe_result = if let (Some(ip), Some(inner)) = (iso_p, inner_p) {
            probe::probe_iso_file_with_timeout(ip, inner, probe_timeout)
        } else {
            probe::probe_file_with_timeout(file, probe_timeout)
        };

        match probe_result {
            Ok(info) => {
                if is_too_short(info.duration_secs, cli.min_duration) {
                    eprintln!(
                        "  skip: {}: duration too short ({:.2}s)",
                        file.display(),
                        info.duration_secs
                    );
                    skipped += 1;
                    continue;
                }
                if probe::meets_target(&info, &cfg.target) {
                    skipped += 1;
                } else {
                    let source_size = if let Some(ref ip) = iso_p {
                        // For ISO entries with multiple files, sum all file sizes
                        if let Some(ref paths) = inner_ps {
                            paths
                                .iter()
                                .filter_map(|p| iso::file_size(ip, p).ok())
                                .sum()
                        } else if let Some(ref inner) = inner_p {
                            iso::file_size(ip, inner).unwrap_or(0)
                        } else {
                            0
                        }
                    } else {
                        std::fs::metadata(file).map(|m| m.len()).unwrap_or(0)
                    };

                    // For multi-file ISO features the initial probe only saw the
                    // representative (first) inner file. The first VOB on a DVD is
                    // often a low-bitrate intro/credits sequence; using its bitrate
                    // as the `-maxrate` cap for the entire concatenated stream
                    // would throttle later, higher-bitrate scenes and degrade
                    // quality. Probe each additional inner file and take the
                    // maximum bitrate (and sum durations for a more accurate
                    // estimate). N extra ffprobe calls per disc is a few seconds
                    // — small price for correct rate-control.
                    let mut extra_probes: Vec<Option<probe::MediaInfo>> = Vec::new();
                    if let (Some(ref ip), Some(ref paths)) = (iso_p, inner_ps) {
                        if paths.len() > 1 {
                            // Skip index 0 — it's the representative we already probed.
                            for inner in paths.iter().skip(1) {
                                match probe::probe_iso_file(ip, inner) {
                                    Ok(extra) => extra_probes.push(Some(extra)),
                                    Err(e) => {
                                        log::debug!(
                                            "  per-file probe failed for {}:{}: {}",
                                            ip.display(),
                                            inner,
                                            e
                                        );
                                        extra_probes.push(None);
                                    }
                                }
                            }
                        }
                    }

                    let multi_file_count = inner_ps.as_ref().map(|p| p.len()).unwrap_or(0);
                    let (bitrate_kbps, duration_secs) =
                        aggregate_iso_probes(&info, &extra_probes, multi_file_count);

                    // Resume / adopt: check if .transcoded output already exists.
                    // Output path logic must match what the worker thread computes:
                    //   - multi-file ISO  → use ISO path as stem
                    //   - single-file ISO → use inner filename as stem
                    //   - regular file    → use file path as stem
                    {
                        let out_dir = cli.output_dir.as_deref().or(cfg.output_dir.as_deref());
                        let source_for_output =
                            output_stem_for_item(file, inner_p.as_deref(), inner_ps.as_deref());
                        if let Ok(out_path) = transcode::output_path(
                            &source_for_output,
                            out_dir,
                            &cfg.target.container,
                        ) {
                            if transcode::output_already_valid(&out_path, file, duration_secs) {
                                // Never rename a .transcoded file back over a disc image —
                                // the ISO/IMG is the source, not the destination.
                                let is_disc = iso_p.is_some();
                                if overwrite && !is_disc {
                                    // Adopt: replace original with existing transcoded file
                                    match transcode::replace_original(
                                        file,
                                        &out_path,
                                        duration_secs,
                                    ) {
                                        Ok(_saved) => {
                                            eprintln!(
                                                "  Replaced {:?} with existing transcoded copy",
                                                file.file_name().unwrap_or_default()
                                            );
                                            resumed += 1;
                                            continue;
                                        }
                                        Err(e) => {
                                            eprintln!(
                                                "  Failed to replace {:?}: {}",
                                                file.file_name().unwrap_or_default(),
                                                e
                                            );
                                            // Fall through to re-transcode
                                        }
                                    }
                                } else {
                                    resumed += 1;
                                    continue;
                                }
                            }
                        }
                    }

                    // Pre-flight: confirm the directory we're about to write
                    // into is actually writable before burning GPU time.
                    //
                    // Selection has to mirror the worker exactly. The worker's
                    // output_path is built like:
                    //   - overwrite && !iso  →  None (in-place: tmp in source's
                    //     parent, then rename over source). output_dir is
                    //     IGNORED in this case — checking it would falsely
                    //     skip valid work when it's set but unwritable.
                    //   - else, with output_dir set  →  output_dir
                    //   - else, no output_dir         →  source's parent
                    //
                    // When output_dir is set, the worker's `output_path` will
                    // `create_dir_all` it before writing, so we mkdir here
                    // too so the writable probe matches reality (otherwise
                    // ENOENT skips a dir that would have worked).
                    //
                    // Skipped entirely under --dry-run: the probe creates and
                    // deletes a file in the user's media tree, which violates
                    // dry-run's "touch nothing" contract. CI / scripted
                    // dry-run callers can preview the plan without surprise IO.
                    if !cli.dry_run {
                        let out_dir = cli.output_dir.as_deref().or(cfg.output_dir.as_deref());
                        let dest_dir: PathBuf = if overwrite && iso_p.is_none() {
                            // In-place mode: worker ignores output_dir and writes
                            // next to the source.
                            file.parent()
                                .map(|p| p.to_path_buf())
                                .unwrap_or_else(|| PathBuf::from("."))
                        } else if let Some(d) = out_dir {
                            // Worker will create_dir_all this path before
                            // writing; do the same so the probe doesn't
                            // ENOENT on a dir that's about to exist.
                            if !d.exists() {
                                if let Err(e) = std::fs::create_dir_all(d) {
                                    let name =
                                        file.file_name().unwrap_or_default().to_string_lossy();
                                    eprintln!(
                                        "  skip: {}: cannot create output directory {:?}: {}",
                                        name, d, e
                                    );
                                    skipped += 1;
                                    continue;
                                }
                            }
                            d.to_path_buf()
                        } else {
                            // Both ISO with no output_dir and non-ISO no-overwrite
                            // land here: <source_parent>/<stem>.transcoded.<ext>.
                            file.parent()
                                .map(|p| p.to_path_buf())
                                .unwrap_or_else(|| PathBuf::from("."))
                        };
                        if !dir_is_writable_cached(&mut writable_cache, &dest_dir) {
                            let name = file.file_name().unwrap_or_default().to_string_lossy();
                            if overwrite && iso_p.is_none() {
                                eprintln!(
                                    "  skip: {}: source directory is not writable; \
                                     use --no-overwrite to write transcodes elsewhere",
                                    name
                                );
                            } else {
                                eprintln!(
                                    "  skip: {}: output directory {:?} is not writable",
                                    name, dest_dir
                                );
                            }
                            skipped += 1;
                            continue;
                        }
                    }

                    to_transcode.push(WorkItem {
                        path: file.clone(),
                        bitrate_kbps,
                        duration_secs,
                        pix_fmt: info.pix_fmt,
                        source_size,
                        iso_path: iso_p.clone(),
                        inner_path: inner_p.clone(),
                        inner_paths: inner_ps.clone(),
                    });
                }
            }
            Err(e) => {
                eprintln!("  skip: {:?}: {}", file.file_name().unwrap_or_default(), e);
                errors += 1;
            }
        }
    }

    if to_transcode.is_empty() {
        if resumed > 0 {
            eprintln!(
                "Nothing to do: {} already HEVC, {} already transcoded",
                skipped, resumed
            );
        } else {
            eprintln!("All {} files already meet target.", expanded.len());
        }
        return Ok(());
    }

    let jobs_specified = cli.jobs.is_some();
    // Auto mode: spawn extra threads to probe beyond the theoretical limit
    let jobs = cli
        .jobs
        .unwrap_or_else(|| gpu::max_encode_sessions(&gpu) + 3)
        .max(1);
    let auto_ramp = !jobs_specified;

    eprintln!(
        "{} to transcode, {} already HEVC{}, {}",
        to_transcode.len(),
        skipped,
        if resumed > 0 {
            format!(", {} resumed", resumed)
        } else {
            String::new()
        },
        if auto_ramp {
            "auto concurrency".to_string()
        } else {
            format!("{} jobs", jobs)
        },
    );

    if cli.dry_run {
        for item in &to_transcode {
            let name = item.path.file_name().unwrap_or_default().to_string_lossy();
            eprintln!(
                "  {name} ({}, {} kbps, {})",
                item.pix_fmt,
                item.bitrate_kbps,
                format_size(item.source_size)
            );
        }
        let total_size: u64 = to_transcode.iter().map(|i| i.source_size).sum();
        eprintln!(
            "\nDry run: {} would be transcoded ({})",
            to_transcode.len(),
            format_size(total_size)
        );
        return Ok(());
    }

    // Register work directories for CTRL-C cleanup
    {
        let mut dirs = TMP_DIRS.lock().unwrap();
        for item in &to_transcode {
            if let Some(parent) = item.path.parent() {
                if !dirs.contains(&parent.to_path_buf()) {
                    dirs.push(parent.to_path_buf());
                }
            }
        }
    }

    // --- Phase 3: Transcode with live viewport rendering ---
    let total_units = to_transcode.len() as u64 * 1000;
    let file_count = to_transcode.len() as u64;

    let worker_slots: Vec<Arc<WorkerSlot>> = (0..jobs)
        .map(|_| {
            Arc::new(WorkerSlot {
                info: Mutex::new(None),
                progress: AtomicU64::new(0),
                speed: AtomicU64::new(0),
                queued: AtomicBool::new(false),
                disk_wait: AtomicBool::new(false),
            })
        })
        .collect();
    let completed_lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let completed_units = Arc::new(AtomicU64::new(0));

    // Encoding limiter: discovers actual GPU session capacity at runtime
    let active_encoders = Arc::new(AtomicU32::new(0));
    let initial_max = if auto_ramp { 1u32 } else { jobs as u32 };
    let max_encoders = Arc::new(AtomicU32::new(initial_max));
    // Whether we're still probing for the GPU's actual session limit
    let ramping = Arc::new(AtomicBool::new(auto_ramp));
    // How many workers are still alive (decremented on retirement)
    let worker_count = Arc::new(AtomicU32::new(jobs as u32));
    // Cumulative NVENC session-limit hits across the entire run.  After
    // MAX_SESSION_LIMIT_BEFORE_FREEZE we permanently disable ramping and
    // pin max_encoders at the lowest observed working concurrency.
    let session_limit_hits = Arc::new(AtomicU32::new(0));
    let session_limit_frozen = Arc::new(AtomicBool::new(false));
    // Lowest concurrency that successfully ran an encode after a session-limit
    // hit.  Initialised to u32::MAX as a sentinel meaning "no observation yet".
    let min_observed_max = Arc::new(AtomicU32::new(u32::MAX));

    // Disk space limiter: tracks estimated bytes reserved by in-flight encodes.
    // We estimate output at source_size/2 (conservative for HEVC compression).
    // 2GB margin prevents filesystem from getting dangerously full.
    let disk_reserved = Arc::new(AtomicU64::new(0));
    const DISK_MARGIN: u64 = 2 * 1024 * 1024 * 1024;

    let transcoded = Arc::new(AtomicU32::new(0));
    let error_count = Arc::new(AtomicU32::new(errors));
    let bytes_saved = Arc::new(AtomicU64::new(0));
    let bytes_input = Arc::new(AtomicU64::new(0));
    let bytes_output = Arc::new(AtomicU64::new(0));
    let to_transcode = Arc::new(to_transcode);
    let next_idx = Arc::new(AtomicU32::new(0));
    let cfg = Arc::new(cfg);
    let gpu = Arc::new(gpu);
    let output_dir = cli.output_dir.clone();

    std::thread::scope(|s| {
        // Render thread (also controls auto-ramp)
        {
            let render_slots: Vec<Arc<WorkerSlot>> = worker_slots.iter().map(Arc::clone).collect();
            let render_completed = Arc::clone(&completed_lines);
            let render_completed_units = Arc::clone(&completed_units);
            let render_transcoded = Arc::clone(&transcoded);
            let render_errors = Arc::clone(&error_count);
            let render_max = Arc::clone(&max_encoders);
            let render_ramping = Arc::clone(&ramping);
            let render_frozen = Arc::clone(&session_limit_frozen);

            s.spawn(move || {
                let start = Instant::now();
                let mut prev_viewport = 0usize;

                // Auto-ramp state
                let mut ramp_baseline_speed = 0u64;
                let mut last_ramp_time = start;

                loop {
                    {
                        let mut stderr = std::io::stderr().lock();

                        // Cursor is on the last viewport line (no trailing \n).
                        // \r goes to column 0, then move up (viewport-1) to
                        // reach the first viewport line, then erase to end of
                        // screen.  Omitting the trailing newline on the progress
                        // bar prevents the viewport from scrolling into the
                        // scrollback buffer — which is what causes ghost progress
                        // lines in the terminal history.
                        if prev_viewport > 0 {
                            write!(stderr, "\r").ok();
                            if prev_viewport > 1 {
                                write!(stderr, "\x1b[{}A", prev_viewport - 1).ok();
                            }
                        }
                        write!(stderr, "\x1b[J").ok();

                        {
                            let mut lines = render_completed.lock().unwrap();
                            for line in lines.drain(..) {
                                writeln!(stderr, "{}", line).ok();
                            }
                        }

                        let mut viewport = 0;
                        let max = render_max.load(Ordering::Relaxed);

                        for (slot_idx, slot) in render_slots.iter().enumerate() {
                            let info = slot.info.lock().unwrap();
                            if let Some((ref name, ref size)) = *info {
                                let is_excess = (slot_idx + 1) as u32 > max;
                                if slot.disk_wait.load(Ordering::Relaxed) {
                                    writeln!(
                                        stderr,
                                        "  {}           {} ({})  waiting for disk",
                                        sym.hourglass, name, size,
                                    )
                                    .ok();
                                } else if slot.queued.load(Ordering::Relaxed) && is_excess {
                                    writeln!(
                                        stderr,
                                        "  {}           {} ({})  queued {}/{}",
                                        sym.hourglass,
                                        name,
                                        size,
                                        slot_idx + 1,
                                        max
                                    )
                                    .ok();
                                } else {
                                    let pct = slot.progress.load(Ordering::Relaxed) / 10;
                                    let spd = slot.speed.load(Ordering::Relaxed);
                                    let speed_str = if spd > 0 {
                                        format!("{}.{}x", spd / 100, (spd % 100) / 10)
                                    } else {
                                        String::new()
                                    };
                                    writeln!(
                                        stderr,
                                        "  {} {:>2}% {:>4} {} ({})",
                                        sym.play, pct, speed_str, name, size
                                    )
                                    .ok();
                                }
                                viewport += 1;
                            }
                        }

                        // Progress bar
                        let done_units = render_completed_units.load(Ordering::Relaxed);
                        let active_sum: u64 = render_slots
                            .iter()
                            .map(|s| s.progress.load(Ordering::Relaxed))
                            .sum();
                        let current = (done_units + active_sum).min(total_units);
                        let frac = if total_units > 0 {
                            current as f64 / total_units as f64
                        } else {
                            0.0
                        };

                        let completed = render_transcoded.load(Ordering::Relaxed);
                        let errs = render_errors.load(Ordering::Relaxed);
                        let finished = completed + errs;
                        let elapsed = start.elapsed().as_secs();

                        // No trailing newline — keeps the cursor ON the
                        // progress bar so the viewport never scrolls into
                        // the scrollback buffer.
                        write!(
                            stderr,
                            "  {} {}/{} done  [{:02}:{:02}:{:02}]",
                            progress_bar_str(frac, 40, sym),
                            finished,
                            file_count,
                            elapsed / 3600,
                            (elapsed % 3600) / 60,
                            elapsed % 60
                        )
                        .ok();
                        viewport += 1;

                        stderr.flush().ok();
                        prev_viewport = viewport;
                    }

                    // --- Auto-ramp: add workers while total throughput improves ---
                    // If we've permanently frozen the max after repeated NVENC
                    // session-limit hits, force ramping off and skip entirely.
                    if render_frozen.load(Ordering::SeqCst) {
                        render_ramping.store(false, Ordering::SeqCst);
                    }
                    if render_ramping.load(Ordering::SeqCst)
                        && !render_frozen.load(Ordering::SeqCst)
                        && last_ramp_time.elapsed().as_secs() >= 5
                    {
                        let current_max = render_max.load(Ordering::SeqCst);

                        // Collect speeds from active (non-queued) workers
                        let speeds: Vec<u64> = render_slots
                            .iter()
                            .filter(|s| {
                                s.info.lock().unwrap().is_some()
                                    && !s.queued.load(Ordering::Relaxed)
                            })
                            .map(|s| s.speed.load(Ordering::Relaxed))
                            .collect();
                        let reporting = speeds.iter().filter(|&&s| s > 0).count() as u32;
                        let total_speed: u64 = speeds.iter().sum();

                        // Wait until all current slots are encoding and reporting speed
                        if reporting >= current_max && total_speed > 0 {
                            if ramp_baseline_speed == 0 {
                                // First measurement: record baseline and ramp
                                ramp_baseline_speed = total_speed;
                                render_max.store(current_max + 1, Ordering::SeqCst);
                                last_ramp_time = Instant::now();
                            } else if total_speed > ramp_baseline_speed {
                                // Total throughput improved: keep ramping
                                ramp_baseline_speed = total_speed;
                                render_max.store(current_max + 1, Ordering::SeqCst);
                                last_ramp_time = Instant::now();
                            } else {
                                // Throughput stalled or dropped: stop ramping
                                render_ramping.store(false, Ordering::SeqCst);
                                if total_speed < ramp_baseline_speed * 85 / 100 {
                                    // Significant drop: revert last ramp
                                    lower_max(&render_max, current_max.saturating_sub(1).max(1));
                                }
                            }
                        }
                    }

                    let completed = render_transcoded.load(Ordering::Relaxed);
                    let errs = render_errors.load(Ordering::Relaxed);
                    let cancelled = CANCELLED.load(Ordering::Relaxed);
                    if (completed + errs) as u64 >= file_count || cancelled {
                        let mut stderr = std::io::stderr().lock();
                        if prev_viewport > 0 {
                            write!(stderr, "\r").ok();
                            if prev_viewport > 1 {
                                write!(stderr, "\x1b[{}A", prev_viewport - 1).ok();
                            }
                            write!(stderr, "\x1b[J").ok();
                        }
                        write!(stderr, "\x1b[0m\r\n").ok();
                        stderr.flush().ok();
                        break;
                    }

                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            });
        }

        // Worker threads. worker_slots was built with exactly `jobs` entries
        // (see WorkerSlot construction above), so iterating it spawns one
        // thread per slot.
        for slot in &worker_slots {
            let to_transcode = Arc::clone(&to_transcode);
            let next_idx = Arc::clone(&next_idx);
            let transcoded = Arc::clone(&transcoded);
            let error_count = Arc::clone(&error_count);
            let bytes_saved = Arc::clone(&bytes_saved);
            let bytes_input = Arc::clone(&bytes_input);
            let bytes_output = Arc::clone(&bytes_output);
            let cfg = Arc::clone(&cfg);
            let gpu = Arc::clone(&gpu);
            let output_dir = output_dir.clone();
            let completed_units = Arc::clone(&completed_units);
            let completed_lines = Arc::clone(&completed_lines);
            let my_slot = Arc::clone(slot);
            let active_encoders = Arc::clone(&active_encoders);
            let max_encoders = Arc::clone(&max_encoders);
            let ramping = Arc::clone(&ramping);
            let worker_count = Arc::clone(&worker_count);
            let disk_reserved = Arc::clone(&disk_reserved);
            let session_limit_hits = Arc::clone(&session_limit_hits);
            let session_limit_frozen = Arc::clone(&session_limit_frozen);
            let min_observed_max = Arc::clone(&min_observed_max);

            s.spawn(move || 'outer: loop {
                if CANCELLED.load(Ordering::Relaxed) {
                    break;
                }

                let idx = next_idx.fetch_add(1, Ordering::Relaxed) as usize;
                if idx >= to_transcode.len() {
                    break;
                }

                let item = &to_transcode[idx];
                let name = if let Some(ref paths) = item.inner_paths {
                    // Multi-file ISO: show "iso_name (N files)"
                    let iso_name = item.path.file_name().unwrap_or_default().to_string_lossy();
                    format!("{} ({} files)", iso_name, paths.len())
                } else if let Some(ref inner) = item.inner_path {
                    // Single-file ISO: show "iso_name:inner_name"
                    let iso_name = item.path.file_name().unwrap_or_default().to_string_lossy();
                    let inner_name = Path::new(inner)
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy();
                    format!("{}:{}", iso_name, inner_name)
                } else {
                    item.path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string()
                };
                let short_name = truncate_name(&name, max_name, sym);
                let size_str = format_size(item.source_size);

                // In fixed mode (-j specified), show file immediately.
                // In auto mode, stay invisible until we acquire an encoding slot.
                if !auto_ramp {
                    let mut info = my_slot.info.lock().unwrap();
                    *info = Some((short_name.clone(), size_str.clone()));
                    my_slot.progress.store(0, Ordering::Relaxed);
                    my_slot.speed.store(0, Ordering::Relaxed);
                }

                let output_path = if overwrite && item.iso_path.is_none() {
                    None
                } else {
                    let out_dir = output_dir.as_deref().or(cfg.output_dir.as_deref());
                    let source_for_output = output_stem_for_item(
                        &item.path,
                        item.inner_path.as_deref(),
                        item.inner_paths.as_deref(),
                    );
                    match transcode::output_path(&source_for_output, out_dir, &cfg.target.container)
                    {
                        Ok(p) => Some(p),
                        Err(e) => {
                            completed_lines
                                .lock()
                                .unwrap()
                                .push(format!("  {} {short_name}: {e}", sym.cross));
                            error_count.fetch_add(1, Ordering::Relaxed);
                            *my_slot.info.lock().unwrap() = None;
                            completed_units.fetch_add(1000, Ordering::Relaxed);
                            continue;
                        }
                    }
                };

                // Estimate output size for disk reservation (conservative: 50% of source)
                let disk_estimate = (item.source_size / 2).max(100 * 1024 * 1024);

                // Acquire encoding slot + encode with session-limit retry
                let mut session_retries = 0u32;
                let mut disk_space_retries = 0u32;
                let mut skip_subs = false;
                let mut force_reencode_audio = false;
                // None → use configured codec (typically "copy"). Some(c) → override
                // with `c` (the subtitle re-encode retry tier between copy and skip).
                let mut subtitle_reencode_attempt: Option<&'static str> = None;

                let last_err: Option<anyhow::Error> = loop {
                    if CANCELLED.load(Ordering::Relaxed) {
                        break None;
                    }

                    // Check disk space before acquiring GPU slot.
                    // Determine the output filesystem path for the check.
                    let check_dir = output_path
                        .as_deref()
                        .and_then(|p| p.parent())
                        .unwrap_or_else(|| item.path.parent().unwrap_or(Path::new("/")));

                    let has_disk = if let Ok(avail) = util::available_disk_space(check_dir) {
                        let reserved = disk_reserved.load(Ordering::SeqCst);
                        let effective = avail.saturating_sub(reserved);
                        // Always allow at least 1 encode (avoid deadlock when disk is tight)
                        reserved == 0 || effective >= disk_estimate + DISK_MARGIN
                    } else {
                        true // Can't check? Proceed optimistically
                    };

                    if !has_disk {
                        // Show disk wait status
                        my_slot.disk_wait.store(true, Ordering::Relaxed);
                        if !auto_ramp {
                            let mut info = my_slot.info.lock().unwrap();
                            *info = Some((short_name.clone(), size_str.clone()));
                        }
                        std::thread::sleep(std::time::Duration::from_millis(2000));
                        continue;
                    }
                    my_slot.disk_wait.store(false, Ordering::Relaxed);

                    // Wait for an encoding slot
                    if !try_acquire(&active_encoders, &max_encoders) {
                        // In fixed mode, show queued status for excess workers
                        if !auto_ramp {
                            my_slot.queued.store(true, Ordering::Relaxed);
                            my_slot.progress.store(0, Ordering::Relaxed);
                            my_slot.speed.store(0, Ordering::Relaxed);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        continue;
                    }
                    my_slot.queued.store(false, Ordering::Relaxed);

                    // Reserve disk space
                    disk_reserved.fetch_add(disk_estimate, Ordering::SeqCst);

                    // In auto mode, become visible now that we have a slot
                    if auto_ramp {
                        let mut info = my_slot.info.lock().unwrap();
                        *info = Some((short_name.clone(), size_str.clone()));
                        my_slot.progress.store(0, Ordering::Relaxed);
                        my_slot.speed.store(0, Ordering::Relaxed);
                    }

                    let encode_result = if let Some(ref iso) = item.iso_path {
                        // ISO entry: stream from disc image to ffmpeg
                        let out = output_path
                            .as_deref()
                            .expect("ISO entries always need an output path");
                        // Use inner_paths (multi-file concat) or single inner_path
                        let paths = item
                            .inner_paths
                            .clone()
                            .or_else(|| item.inner_path.as_ref().map(|p| vec![p.clone()]))
                            .unwrap_or_default();
                        transcode::transcode_iso(
                            iso,
                            &paths,
                            out,
                            &cfg.target,
                            &gpu,
                            item.bitrate_kbps,
                            item.duration_secs,
                            &item.pix_fmt,
                            Some(&my_slot.progress),
                            Some(&my_slot.speed),
                            skip_subs,
                            force_reencode_audio,
                            subtitle_reencode_attempt,
                        )
                    } else {
                        transcode::transcode(
                            &item.path,
                            output_path.as_deref(),
                            &cfg.target,
                            &gpu,
                            item.bitrate_kbps,
                            item.duration_secs,
                            &item.pix_fmt,
                            Some(&my_slot.progress),
                            Some(&my_slot.speed),
                            skip_subs,
                            force_reencode_audio,
                            subtitle_reencode_attempt,
                        )
                    };

                    match encode_result {
                        Ok(out_path) => {
                            active_encoders.fetch_sub(1, Ordering::SeqCst);
                            disk_reserved.fetch_sub(disk_estimate, Ordering::SeqCst);

                            let out_size = transcode::output_size(&out_path);
                            bytes_input.fetch_add(item.source_size, Ordering::Relaxed);
                            bytes_output.fetch_add(out_size, Ordering::Relaxed);
                            if item.source_size > out_size {
                                bytes_saved
                                    .fetch_add(item.source_size - out_size, Ordering::Relaxed);
                            }
                            transcoded.fetch_add(1, Ordering::Relaxed);
                            let saved_pct = if item.source_size > 0 {
                                ((item.source_size as f64 - out_size as f64)
                                    / item.source_size as f64
                                    * 100.0) as i32
                            } else {
                                0
                            };

                            // Deactivate slot BEFORE pushing completed line to avoid
                            // render thread showing both the active slot and the ✓ line.
                            *my_slot.info.lock().unwrap() = None;
                            my_slot.progress.store(0, Ordering::Relaxed);
                            my_slot.speed.store(0, Ordering::Relaxed);

                            completed_lines.lock().unwrap().push(format!(
                                "  {} {} ({} {} {}, -{}%)",
                                sym.check,
                                short_name,
                                format_size(item.source_size),
                                sym.arrow,
                                format_size(out_size),
                                saved_pct
                            ));
                            if stdout_is_pipe {
                                println!("{}", out_path.display());
                            }
                            break None;
                        }
                        Err(e) => {
                            active_encoders.fetch_sub(1, Ordering::SeqCst);
                            disk_reserved.fetch_sub(disk_estimate, Ordering::SeqCst);
                            let err_str = e.to_string();

                            // Disk space error: wait for other encodes to free space, then retry
                            if transcode::is_disk_space_error(&err_str)
                                && disk_space_retries < MAX_SESSION_RETRIES
                            {
                                disk_space_retries += 1;
                                my_slot.disk_wait.store(true, Ordering::Relaxed);
                                *my_slot.info.lock().unwrap() =
                                    Some((short_name.clone(), size_str.clone()));
                                my_slot.progress.store(0, Ordering::Relaxed);
                                my_slot.speed.store(0, Ordering::Relaxed);
                                std::thread::sleep(std::time::Duration::from_secs(5));
                                my_slot.disk_wait.store(false, Ordering::Relaxed);
                                continue;
                            }

                            // Audio-copy error: matroska / wav-tag muxer rejecting a
                            // copied stream (most commonly DVDs' pcm_dvd into MKV).
                            // Checked BEFORE the subtitle/Nothing-written branch because
                            // both produce a "Nothing was written" cascade — but here
                            // the fix is re-encoding audio, not dropping subtitles.
                            if transcode::is_audio_copy_error(&err_str) && !force_reencode_audio {
                                force_reencode_audio = true;
                                log::info!(
                                    "{}: retrying with audio re-encode (source codec not muxable as copy)",
                                    short_name
                                );
                                my_slot.progress.store(0, Ordering::Relaxed);
                                my_slot.speed.store(0, Ordering::Relaxed);
                                continue;
                            }

                            // Subtitle error: tiered retry.
                            // Checked BEFORE session-limit: "Nothing was written" can also
                            // mean a subtitle codec is incompatible with the container
                            // (e.g. mov_text → MKV), which silently produces no output.
                            // Tier 1 (already attempted): config codec, typically `copy`.
                            // Tier 2: re-encode subs to a container-appropriate text codec
                            //         (`srt` for mkv, `mov_text` for mp4). This preserves
                            //         text-source subs while a problematic bitmap track
                            //         (PGS/dvdsub) still fails through to tier 3.
                            // Tier 3: drop subs entirely (existing skip_subs fallback).
                            let nothing_written =
                                err_str.contains("Nothing was written into output file");
                            if (transcode::is_subtitle_error(&err_str) || nothing_written)
                                && subtitle_reencode_attempt.is_none()
                                && !skip_subs
                            {
                                let codec = transcode::subtitle_reencode_fallback(
                                    &cfg.target.container,
                                );
                                subtitle_reencode_attempt = Some(codec);
                                log::info!(
                                    "{}: retrying with subtitle re-encode (-c:s {})",
                                    short_name,
                                    codec
                                );
                                my_slot.progress.store(0, Ordering::Relaxed);
                                my_slot.speed.store(0, Ordering::Relaxed);
                                continue;
                            }
                            if (transcode::is_subtitle_error(&err_str) || nothing_written)
                                && !skip_subs
                            {
                                skip_subs = true;
                                // Once we drop subs entirely, the override is moot.
                                subtitle_reencode_attempt = None;
                                log::info!("{}: retrying without subtitles", short_name);
                                my_slot.progress.store(0, Ordering::Relaxed);
                                my_slot.speed.store(0, Ordering::Relaxed);
                                continue;
                            }

                            if transcode::is_session_limit_error(&err_str)
                                && session_retries < MAX_SESSION_RETRIES
                            {
                                session_retries += 1;
                                // Stop ramping — we found the GPU's limit
                                ramping.store(false, Ordering::SeqCst);
                                // Lower the discovered max to current active count
                                let active = active_encoders.load(Ordering::SeqCst).max(1);
                                lower_max(&max_encoders, active);

                                // Track the lowest concurrency we've ever
                                // had to fall back to during a session-limit
                                // event.  After repeated hits we'll pin the
                                // max here permanently.
                                min_observed_max.fetch_min(active, Ordering::SeqCst);

                                // Count this hit across the whole run.  Once
                                // we cross the freeze threshold, ramping is
                                // permanently disabled and the max is pinned
                                // at the lowest observed working value.
                                let hits =
                                    session_limit_hits.fetch_add(1, Ordering::SeqCst) + 1;
                                if should_freeze(hits)
                                    && !session_limit_frozen.swap(true, Ordering::SeqCst)
                                {
                                    let pin = min_observed_max
                                        .load(Ordering::SeqCst)
                                        .min(active)
                                        .max(1);
                                    lower_max(&max_encoders, pin);
                                    ramping.store(false, Ordering::SeqCst);
                                    log::info!(
                                        "NVENC session limit hit {} times; \
                                         freezing max parallel encoders at {} \
                                         for the rest of this run.",
                                        hits,
                                        pin,
                                    );
                                }

                                // Hide this slot while retrying
                                *my_slot.info.lock().unwrap() = None;
                                my_slot.progress.store(0, Ordering::Relaxed);
                                my_slot.speed.store(0, Ordering::Relaxed);
                                continue;
                            }

                            break Some(e);
                        }
                    }
                };

                // Deactivate slot before pushing status lines to avoid
                // render thread showing both the active slot and the status line.
                *my_slot.info.lock().unwrap() = None;
                my_slot.progress.store(0, Ordering::Relaxed);
                my_slot.speed.store(0, Ordering::Relaxed);
                my_slot.queued.store(false, Ordering::Relaxed);
                my_slot.disk_wait.store(false, Ordering::Relaxed);

                if let Some(e) = last_err {
                    completed_lines
                        .lock()
                        .unwrap()
                        .push(format!("  {} {short_name}: {e}", sym.cross));
                    error_count.fetch_add(1, Ordering::Relaxed);
                }

                completed_units.fetch_add(1000, Ordering::Relaxed);

                // Retire excess workers: if we discovered a lower capacity,
                // workers beyond that count exit after finishing their file.
                let max = max_encoders.load(Ordering::SeqCst);
                if max < jobs as u32 {
                    let wc = worker_count.load(Ordering::SeqCst);
                    if wc > max
                        && worker_count
                            .compare_exchange(wc, wc - 1, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok()
                    {
                        break 'outer;
                    }
                }
            });
        }
    });

    // Drain any completed lines the render thread didn't get to (skip on cancel)
    if !CANCELLED.load(Ordering::Relaxed) {
        let lines = completed_lines.lock().unwrap();
        for line in lines.iter() {
            eprintln!("{}", line);
        }
    }

    // Clean up any leftover .hvac_tmp_* files
    cleanup_tmp_dirs();

    let was_cancelled = CANCELLED.load(Ordering::Relaxed);

    if was_cancelled {
        eprintln!(
            "\nCancelled: {} transcoded before interrupt",
            transcoded.load(Ordering::Relaxed)
        );
        std::process::exit(130);
    }

    // --- Phase 4: Replace originals if --replace ---
    let mut replaced = 0u32;
    let mut replace_saved: u64 = 0;

    if cli.replace && !overwrite {
        eprintln!("Replacing originals with transcoded copies...");
        for item in to_transcode.iter() {
            let out_dir = cli.output_dir.as_deref().or(cfg.output_dir.as_deref());
            if let Ok(out_path) = transcode::output_path(&item.path, out_dir, &cfg.target.container)
            {
                if out_path.exists() {
                    match transcode::replace_original(&item.path, &out_path, item.duration_secs) {
                        Ok(saved) => {
                            replaced += 1;
                            replace_saved += saved;
                        }
                        Err(e) => {
                            eprintln!(
                                "  {} replace {:?}: {}",
                                sym.cross,
                                item.path.file_name().unwrap_or_default(),
                                e
                            );
                        }
                    }
                }
            }
        }
        if replaced > 0 {
            eprintln!(
                "Replaced {} originals (saved {})",
                replaced,
                format_size(replace_saved)
            );
        }
    }

    let final_transcoded = transcoded.load(Ordering::Relaxed);
    let final_errors = error_count.load(Ordering::Relaxed);
    let total_saved = bytes_saved.load(Ordering::Relaxed);
    let total_input = bytes_input.load(Ordering::Relaxed);
    let total_output = bytes_output.load(Ordering::Relaxed);

    eprintln!(
        "\nDone: {} transcoded, {} skipped, {} errors",
        final_transcoded, skipped, final_errors
    );

    if total_input > 0 {
        let pct = (total_saved as f64 / total_input as f64) * 100.0;
        eprintln!(
            "Size: {} {} {} (saved {}, {:.0}% reduction)",
            format_size(total_input),
            sym.arrow,
            format_size(total_output),
            format_size(total_saved),
            pct
        );
    }

    if final_errors > 0 {
        std::process::exit(1);
    }

    Ok(())
}

/// Remove .hvac_tmp_* files from all registered work directories.
fn cleanup_tmp_dirs() {
    if let Ok(dirs) = TMP_DIRS.lock() {
        for dir in dirs.iter() {
            cleanup_tmp_in_dir(dir);
        }
    }
}

fn cleanup_tmp_in_dir(dir: &std::path::Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if name.starts_with(".hvac_tmp_") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_info(bitrate_kbps: u32, duration_secs: f64) -> probe::MediaInfo {
        probe::MediaInfo {
            codec: "mpeg2video".to_string(),
            width: 720,
            height: 480,
            bitrate_kbps,
            duration_secs,
            pix_fmt: "yuv420p".to_string(),
            has_audio: true,
            has_subtitles: false,
        }
    }

    #[test]
    fn aggregate_iso_probes_single_file_returns_representative() {
        let rep = make_info(4000, 1500.0);
        let (b, d) = aggregate_iso_probes(&rep, &[], 1);
        assert_eq!(b, 4000);
        assert!((d - 1500.0).abs() < 1e-9);
    }

    #[test]
    fn aggregate_iso_probes_zero_count_returns_representative() {
        // Defensive: 0 should also short-circuit to the rep (no-op safe).
        let rep = make_info(4000, 1500.0);
        let (b, d) = aggregate_iso_probes(&rep, &[], 0);
        assert_eq!(b, 4000);
        assert!((d - 1500.0).abs() < 1e-9);
    }

    #[test]
    fn aggregate_iso_probes_takes_max_bitrate_across_inner_files() {
        // First VOB is a 2 Mbps intro; later VOBs are 9 Mbps action scenes.
        let rep = make_info(2000, 60.0);
        let extras = vec![Some(make_info(9000, 600.0)), Some(make_info(7500, 500.0))];
        let (b, _) = aggregate_iso_probes(&rep, &extras, 3);
        assert_eq!(b, 9000, "should pick the max, not the first VOB's bitrate");
    }

    #[test]
    fn aggregate_iso_probes_sums_durations_when_all_probes_succeed() {
        let rep = make_info(2000, 60.0);
        let extras = vec![Some(make_info(9000, 600.0)), Some(make_info(7500, 500.0))];
        let (_, d) = aggregate_iso_probes(&rep, &extras, 3);
        assert!(
            (d - 1160.0).abs() < 1e-9,
            "expected 60+600+500=1160, got {d}"
        );
    }

    #[test]
    fn aggregate_iso_probes_falls_back_to_count_multiplier_on_probe_failure() {
        // If any per-file probe fails the sum would undercount, so we
        // fall back to representative * total_count.
        let rep = make_info(2000, 60.0);
        let extras = vec![Some(make_info(9000, 600.0)), None];
        let (b, d) = aggregate_iso_probes(&rep, &extras, 3);
        // Bitrate max is still computed from successful probes.
        assert_eq!(b, 9000);
        // Duration falls back to 60 * 3 = 180.
        assert!((d - 180.0).abs() < 1e-9, "expected fallback 180, got {d}");
    }

    #[test]
    fn test_should_freeze_threshold() {
        // Below threshold: keep ramping/retrying.
        assert!(!should_freeze(0));
        assert!(!should_freeze(1));
        assert!(!should_freeze(MAX_SESSION_LIMIT_BEFORE_FREEZE - 1));
        // At and above the threshold: freeze permanently.
        assert!(should_freeze(MAX_SESSION_LIMIT_BEFORE_FREEZE));
        assert!(should_freeze(MAX_SESSION_LIMIT_BEFORE_FREEZE + 1));
        assert!(should_freeze(u32::MAX));
    }

    #[test]
    fn test_should_freeze_threshold_is_small() {
        // Sanity: the threshold is meant to be small (a handful of hits),
        // not so high that a long run still cycles indefinitely.
        assert!(MAX_SESSION_LIMIT_BEFORE_FREEZE >= 2);
        assert!(MAX_SESSION_LIMIT_BEFORE_FREEZE <= 10);
    }

    #[test]
    fn test_banner_contains_key_instructions() {
        for use_unicode in [true, false] {
            let banner = embedded_config_banner(use_unicode);
            assert!(
                banner.contains("--dump-config"),
                "banner missing --dump-config"
            );
            assert!(banner.contains("--config"), "banner missing --config");
            assert!(banner.contains("--quiet"), "banner missing --quiet");
            assert!(banner.contains("$EDITOR"), "banner missing $EDITOR");
        }
    }

    #[test]
    fn test_banner_unicode_uses_box_chars() {
        let banner = embedded_config_banner(true);
        assert!(banner.contains('╭'), "unicode banner missing ╭");
        assert!(banner.contains('╯'), "unicode banner missing ╯");
        assert!(!banner.contains('+'), "unicode banner should not contain +");
    }

    #[test]
    fn test_banner_ascii_uses_plain_chars() {
        let banner = embedded_config_banner(false);
        assert!(banner.contains('+'), "ascii banner missing +");
        assert!(!banner.contains('╭'), "ascii banner should not contain ╭");
    }

    #[test]
    fn test_banner_lines_are_equal_width() {
        // Every line in the banner must be the same display width.
        for use_unicode in [true, false] {
            let banner = embedded_config_banner(use_unicode);
            let widths: Vec<usize> = banner.lines().map(|l| l.chars().count()).collect();
            let first = widths[0];
            for (i, w) in widths.iter().enumerate() {
                assert_eq!(
                    *w, first,
                    "line {} has width {} but expected {}",
                    i, w, first
                );
            }
        }
    }

    // ── Line-length regression tests ─────────────────────────────────────────
    //
    // Every format string used in the render loop must produce lines that fit
    // within the terminal width.  If any format string gains extra characters,
    // or if LINE_FORMAT_OVERHEAD is reduced below the true maximum overhead,
    // these tests will catch it before it ships.

    /// Build the worst-case line for each render format and assert they all
    /// fit within `cols`.  Uses ASCII symbols (byte-length == display-width)
    /// and the widest realistic size string ("1000.0GB").
    fn check_line_lengths(cols: usize) {
        let max_name = max_name_for_cols(cols);
        let name = "A".repeat(max_name);
        let size = "1000.0GB";

        // Worker active: "  > XX% X.Xx <name> (<size>)"
        let worker = format!("  {} {:>2}% {:>4} {} ({})", ">", 99, "9.9x", name, size);
        assert!(
            worker.len() <= cols,
            "worker line ({} chars) exceeds {} cols at max_name={}: {:?}",
            worker.len(),
            cols,
            max_name,
            worker
        );

        // Completed: "  + <name> (<src> -> <dst>, -100%)"
        let completed = format!("  {} {} ({} {} {}, -{}%)", "+", name, size, "->", size, 100);
        assert!(
            completed.len() <= cols,
            "completed line ({} chars) exceeds {} cols at max_name={}: {:?}",
            completed.len(),
            cols,
            max_name,
            completed
        );

        // Disk-wait: "  ~           <name> (<size>)  waiting for disk"
        let diskwait = format!("  {}           {} ({})  waiting for disk", "~", name, size);
        assert!(
            diskwait.len() <= cols,
            "disk-wait line ({} chars) exceeds {} cols at max_name={}: {:?}",
            diskwait.len(),
            cols,
            max_name,
            diskwait
        );

        // Queued: "  ~           <name> (<size>)  queued 999/999"
        let queued = format!(
            "  {}           {} ({})  queued {}/{}",
            "~", name, size, 999, 999
        );
        assert!(
            queued.len() <= cols,
            "queued line ({} chars) exceeds {} cols at max_name={}: {:?}",
            queued.len(),
            cols,
            max_name,
            queued
        );
    }

    // ── stdout-interleave regression test ────────────────────────────────────
    //
    // Worker threads must NOT write to stdout when stdout is a terminal.
    // A println! landing between the render thread's cursor-up and \x1b[J
    // corrupts the cursor position and leaves ghost progress lines in the
    // scrollback buffer.  When stdout is a pipe/redirect, writes are fine
    // because they don't go to the same PTY as stderr's escape sequences.

    #[test]
    fn test_stdout_is_pipe_detects_non_tty() {
        // In a test harness stdout is always redirected (not a TTY).
        let result = unsafe { libc::isatty(libc::STDOUT_FILENO) };
        // The test runner redirects stdout, so isatty must return 0.
        assert_eq!(result, 0, "stdout should not be a tty in test environment");
        // Confirm our derived flag
        let stdout_is_pipe = result == 0;
        assert!(stdout_is_pipe);
    }

    #[test]
    fn test_line_lengths_fit_80_cols() {
        check_line_lengths(80);
    }

    #[test]
    fn test_line_lengths_fit_100_cols() {
        check_line_lengths(100);
    }

    #[test]
    fn test_line_lengths_fit_120_cols() {
        check_line_lengths(120);
    }

    #[test]
    fn test_line_lengths_fit_200_cols() {
        check_line_lengths(200);
    }

    #[test]
    fn test_max_name_minimum_is_20() {
        // Even on a very narrow terminal names should be at least 20 chars.
        assert_eq!(max_name_for_cols(0), 20);
        assert_eq!(max_name_for_cols(10), 20);
        assert_eq!(max_name_for_cols(44), 20); // exactly at the boundary
    }

    #[test]
    fn test_max_name_scales_with_width() {
        assert_eq!(max_name_for_cols(80), 36);
        assert_eq!(max_name_for_cols(120), 76);
    }

    // ── ISO output-stem regression tests ─────────────────────────────────────
    //
    // The resume check and the worker thread must compute identical output
    // paths.  Both now call output_stem_for_item(); these tests ensure that
    // function returns the right path for each case.

    #[test]
    fn test_output_stem_regular_file() {
        let file = std::path::Path::new("/media/movie.mkv");
        let stem = output_stem_for_item(file, None, None);
        assert_eq!(stem, file);
    }

    #[test]
    fn test_output_stem_single_file_iso() {
        // Single-file ISO: output uses the ISO filename, not the inner track name.
        // Inner names like "00000.M2TS" or "VTS_01_1.VOB" are meaningless identifiers.
        let iso = std::path::Path::new("/media/Pirates of the Caribbean BR-DISK.iso");
        let inner = "BDMV/STREAM/00000.M2TS";
        let stem = output_stem_for_item(iso, Some(inner), None);
        assert_eq!(stem, iso);
    }

    #[test]
    fn test_output_stem_multi_file_iso() {
        // Multi-file ISO: output is named after the ISO itself, not an inner file.
        let iso = std::path::Path::new("/media/Bliss (1985) DVD.iso");
        let inner = "VIDEO_TS/VTS_01_1.VOB";
        let paths = vec![
            "VIDEO_TS/VTS_01_1.VOB".to_string(),
            "VIDEO_TS/VTS_02_1.VOB".to_string(),
        ];
        let stem = output_stem_for_item(iso, Some(inner), Some(&paths));
        assert_eq!(stem, iso);
    }

    #[test]
    fn test_output_stem_iso_consistent() {
        // Single-file and multi-file ISOs now both use the ISO path,
        // so resume detection is consistent regardless of file count.
        let iso = std::path::Path::new("/media/Movie.iso");
        let inner = "BDMV/STREAM/00000.M2TS";
        let paths = vec![inner.to_string()];
        let single = output_stem_for_item(iso, Some(inner), None);
        let multi = output_stem_for_item(iso, Some(inner), Some(&paths));
        assert_eq!(single, multi, "single and multi-file ISO stems must match");
    }

    // ── Short-duration skip logic ────────────────────────────────────────────

    #[test]
    fn test_is_too_short_below_threshold() {
        // Animated GIF re-muxed as mp4: duration 0.04s — should skip.
        assert!(is_too_short(0.04, 1.0));
        assert!(is_too_short(0.5, 1.0));
        assert!(is_too_short(0.999, 1.0));
    }

    #[test]
    fn test_is_too_short_at_or_above_threshold() {
        // Boundary: duration equal to threshold is NOT too short (strict <).
        assert!(!is_too_short(1.0, 1.0));
        assert!(!is_too_short(1.5, 1.0));
        assert!(!is_too_short(3600.0, 1.0));
    }

    #[test]
    fn test_is_too_short_zero_duration_falls_through() {
        // ffprobe couldn't determine duration → 0.0 → fall through to existing
        // logic (don't silently skip; let the normal path handle it).
        assert!(!is_too_short(0.0, 1.0));
        assert!(!is_too_short(-1.0, 1.0));
    }

    #[test]
    fn test_is_too_short_custom_threshold() {
        // Paranoid user: --min-duration 30 to skip anything under 30 seconds.
        assert!(is_too_short(15.0, 30.0));
        assert!(!is_too_short(45.0, 30.0));

        // Aggressive user: --min-duration 0 disables the skip entirely
        // (no positive duration is < 0).
        assert!(!is_too_short(0.04, 0.0));
        assert!(!is_too_short(1.0, 0.0));
    }

    #[test]
    fn test_min_transcode_duration_default_is_one_second() {
        assert_eq!(MIN_TRANSCODE_DURATION_SECS, 1.0);
    }

    // ── Writable pre-flight tests ────────────────────────────────────────────

    #[test]
    fn test_parent_is_writable_for_normal_temp_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("video.mkv");
        // We don't need the source itself to exist — only its parent.
        assert!(parent_is_writable(&source));
    }

    #[test]
    fn test_parent_is_writable_no_parent() {
        // The root path "/" has no real parent for our purposes (Path::parent
        // returns None for "/" on Unix).
        let root = std::path::Path::new("/");
        assert!(!parent_is_writable(root));
    }

    #[test]
    fn test_dir_is_writable_cached_caches_results() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cache: HashMap<PathBuf, bool> = HashMap::new();
        assert!(dir_is_writable_cached(&mut cache, tmp.path()));
        assert_eq!(cache.len(), 1);
        // Second lookup should hit the cache (same single entry).
        assert!(dir_is_writable_cached(&mut cache, tmp.path()));
        assert_eq!(cache.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn test_parent_is_writable_false_for_readonly_dir() {
        use std::os::unix::fs::PermissionsExt;
        // Skip when running as root: root bypasses DAC permissions and can
        // create files in mode-0o555 directories, which would falsely fail
        // this test.  The deploy host runs tests as root, so we tolerate that.
        let is_root = unsafe { libc::geteuid() } == 0;
        if is_root {
            eprintln!("skipping read-only check under root (DAC bypass)");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let ro_dir = tmp.path().join("ro");
        std::fs::create_dir(&ro_dir).unwrap();
        // r-xr-xr-x: readable + traversable, not writable.
        std::fs::set_permissions(&ro_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let source = ro_dir.join("video.mkv");
        let writable = parent_is_writable(&source);

        // Restore writable perms so tempdir cleanup works.
        let _ = std::fs::set_permissions(&ro_dir, std::fs::Permissions::from_mode(0o755));

        assert!(
            !writable,
            "expected read-only parent to report not writable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_dir_is_writable_cached_remembers_failure() {
        use std::os::unix::fs::PermissionsExt;
        let is_root = unsafe { libc::geteuid() } == 0;
        if is_root {
            eprintln!("skipping read-only cache check under root (DAC bypass)");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let ro_dir = tmp.path().join("ro");
        std::fs::create_dir(&ro_dir).unwrap();
        std::fs::set_permissions(&ro_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let mut cache: HashMap<PathBuf, bool> = HashMap::new();
        let r1 = dir_is_writable_cached(&mut cache, &ro_dir);
        let r2 = dir_is_writable_cached(&mut cache, &ro_dir);

        let _ = std::fs::set_permissions(&ro_dir, std::fs::Permissions::from_mode(0o755));

        assert!(!r1);
        assert!(!r2);
        assert_eq!(cache.get(&ro_dir).copied(), Some(false));
    }

    #[test]
    fn test_dir_is_writable_probe_file_is_cleaned_up() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(dir_is_writable(tmp.path()));
        // After the probe runs, no .hvac_writable_check_* file should remain.
        let leftover: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".hvac_writable_check_")
            })
            .collect();
        assert!(leftover.is_empty(), "probe file should be removed");
    }

    #[test]
    fn test_dir_is_writable_uses_unique_names_per_call() {
        // With nanosecond timestamps + create_new, two back-to-back probes
        // pick distinct filenames — protects against PID-collision clobbering
        // of any user file that happened to be named the same way.
        let tmp = tempfile::tempdir().unwrap();
        // Plant a file with the deterministic-PID-only legacy name. The new
        // probe must not touch it (different name, and create_new wouldn't
        // truncate it even if it did collide).
        let user_file = tmp
            .path()
            .join(format!(".hvac_writable_check_{}", std::process::id()));
        std::fs::write(&user_file, b"user content").unwrap();

        for _ in 0..3 {
            assert!(dir_is_writable(tmp.path()));
        }

        // User's file was not touched.
        assert!(user_file.exists(), "probe must not delete user file");
        assert_eq!(
            std::fs::read(&user_file).unwrap(),
            b"user content",
            "probe must not truncate user file"
        );
    }
}
