mod config;
mod gpu;
mod iso;
mod probe;
mod scanner;
mod transcode;
mod util;

use anyhow::{Context, Result};
use clap::Parser;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use util::format_size;

const MAX_SESSION_RETRIES: u32 = 5;

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

fn detect_symbols() -> &'static Symbols {
    // Check the actual system locale, not just env vars.
    // LANG=en_US.UTF-8 can be set even when the locale isn't installed,
    // causing the C library to fall back to ASCII.
    unsafe {
        // Initialize locale from environment (required before nl_langinfo)
        libc::setlocale(libc::LC_ALL, b"\0".as_ptr() as *const _);
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
#[command(name = "tdorr", version, about = "GPU-accelerated media transcoder")]
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

    /// Overwrite original files instead of creating copies
    #[arg(long, default_value_t = false)]
    overwrite: bool,

    /// Dry run - show what would be transcoded without doing it
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
        "  tdorr: using built-in default configuration",
        "",
        "  To customise encoding settings, save the defaults to a file:",
        "",
        "    tdorr --dump-config > config.yaml",
        "    $EDITOR config.yaml",
        "    tdorr --config config.yaml /path/to/media",
        "",
        "  Suppress this message: tdorr --quiet ...",
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

    // Register CTRL-C handler: first press cancels gracefully, second force-exits
    ctrlc::set_handler(move || {
        // Restore terminal: show cursor, reset attributes, clear to end of screen
        eprint!("\x1b[?25h\x1b[0m");
        if CANCELLED.load(Ordering::Relaxed) {
            eprintln!();
            cleanup_tmp_dirs();
            std::process::exit(130);
        }
        CANCELLED.store(true, Ordering::Relaxed);
        eprintln!("\nCancelling after current encodes finish... (Ctrl-C again to force quit)");
    })
    .ok();

    let sym = detect_symbols();
    let use_unicode = std::ptr::eq(sym, &UNICODE_SYMBOLS);

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

    let files = scanner::scan(path, &cfg.media_extensions)?;

    if files.is_empty() {
        eprintln!("No media files found in {:?}", path);
        return Ok(());
    }

    // --- Phase 1: Expand disc images into flat work list ---
    // Each entry is (path, optional iso_path, optional inner_path, optional inner_paths)
    let mut expanded: Vec<(
        PathBuf,
        Option<PathBuf>,
        Option<String>,
        Option<Vec<String>>,
    )> = Vec::new();
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

    for (file, iso_p, inner_p, inner_ps) in &expanded {
        let probe_result = if let (Some(ip), Some(inner)) = (iso_p, inner_p) {
            probe::probe_iso_file(ip, inner)
        } else {
            probe::probe_file(file)
        };

        match probe_result {
            Ok(info) => {
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

                    // For multi-file ISO features, estimate total duration
                    // by scaling probe duration by file count ratio
                    let duration_secs = if let Some(ref paths) = inner_ps {
                        if paths.len() > 1 {
                            // Probe was for just the first file; scale by count
                            // This is approximate but avoids probing every file
                            info.duration_secs * paths.len() as f64
                        } else {
                            info.duration_secs
                        }
                    } else {
                        info.duration_secs
                    };

                    // Resume / adopt: check if .transcoded output already exists
                    {
                        let out_dir = cli.output_dir.as_deref().or(cfg.output_dir.as_deref());
                        let source_for_output = if let Some(ref inner) = inner_p {
                            let inner_name =
                                std::path::Path::new(inner).file_name().unwrap_or_default();
                            file.parent()
                                .unwrap_or(std::path::Path::new("."))
                                .join(inner_name)
                        } else {
                            file.clone()
                        };
                        if let Ok(out_path) = transcode::output_path(
                            &source_for_output,
                            out_dir,
                            &cfg.target.container,
                        ) {
                            if transcode::output_already_valid(&out_path, file, duration_secs) {
                                if cli.overwrite {
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

                    to_transcode.push(WorkItem {
                        path: file.clone(),
                        bitrate_kbps: info.bitrate_kbps,
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
    let overwrite = cli.overwrite;
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

            s.spawn(move || {
                let start = Instant::now();
                let mut prev_viewport = 0usize;

                // Auto-ramp state
                let mut ramp_baseline_speed = 0u64;
                let mut last_ramp_time = start;

                eprint!("\x1b[?25l");

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
                    if render_ramping.load(Ordering::SeqCst)
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
                        write!(stderr, "\x1b[?25h\x1b[0m").ok();
                        stderr.flush().ok();
                        break;
                    }

                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            });
        }

        // Worker threads
        for worker_id in 0..jobs {
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
            let my_slot = Arc::clone(&worker_slots[worker_id]);
            let active_encoders = Arc::clone(&active_encoders);
            let max_encoders = Arc::clone(&max_encoders);
            let ramping = Arc::clone(&ramping);
            let worker_count = Arc::clone(&worker_count);
            let disk_reserved = Arc::clone(&disk_reserved);

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
                let short_name = truncate_name(&name, 60, sym);
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
                    // For ISO entries: use ISO filename for output (not inner path)
                    // For multi-file features, the ISO name is the natural output name
                    let source_for_output = if item.iso_path.is_some() {
                        if item.inner_paths.is_some() {
                            // Multi-file: use ISO stem as output name
                            item.path.clone()
                        } else if let Some(ref inner) = item.inner_path {
                            let inner_name = Path::new(inner).file_name().unwrap_or_default();
                            item.path
                                .parent()
                                .unwrap_or(Path::new("."))
                                .join(inner_name)
                        } else {
                            item.path.clone()
                        }
                    } else {
                        item.path.clone()
                    };
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
                            println!("{}", out_path.display());
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

                            // Subtitle error: retry without subtitle streams.
                            // Checked BEFORE session-limit: "Nothing was written" can also
                            // mean a subtitle codec is incompatible with the container
                            // (e.g. mov_text → MKV), which silently produces no output.
                            // Try dropping subs first; if that still fails, session-limit
                            // detection runs on the next iteration (skip_subs will be true).
                            let nothing_written =
                                err_str.contains("Nothing was written into output file");
                            if (transcode::is_subtitle_error(&err_str) || nothing_written)
                                && !skip_subs
                            {
                                skip_subs = true;
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

    // Clean up any leftover .tdorr_tmp_* files
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

/// Remove .tdorr_tmp_* files from all registered work directories.
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
                if name.starts_with(".tdorr_tmp_") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
