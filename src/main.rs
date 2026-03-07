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
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use util::format_size;

const MAX_SESSION_RETRIES: u32 = 5;

static CANCELLED: AtomicBool = AtomicBool::new(false);

/// Directories where tmp files may exist — populated before encoding starts,
/// read by the CTRL-C handler for cleanup on force-quit.
static TMP_DIRS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

#[derive(Parser, Debug)]
#[command(name = "tdorr", about = "GPU-accelerated media transcoder")]
struct Cli {
    /// Path to YAML config file
    #[arg(short, long, default_value = "config.yaml")]
    config: PathBuf,

    /// Directory to scan for media files
    path: PathBuf,

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
}

/// Per-worker display slot for the render thread.
struct WorkerSlot {
    info: Mutex<Option<(String, String)>>,
    progress: AtomicU64,
    speed: AtomicU64,
    queued: AtomicBool,
}

fn truncate_name(name: &str, max_len: usize) -> String {
    if name.chars().count() <= max_len {
        name.to_string()
    } else {
        let truncated: String = name.chars().take(max_len - 1).collect();
        format!("{truncated}\u{2026}")
    }
}

fn progress_bar_str(fraction: f64, width: usize) -> String {
    let filled = (fraction * width as f64) as usize;
    if filled >= width {
        "\u{2501}".repeat(width)
    } else {
        format!(
            "{}\u{2578}{}",
            "\u{2501}".repeat(filled),
            "\u{2500}".repeat(width.saturating_sub(filled + 1))
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

fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    // Register CTRL-C handler: first press cancels gracefully, second force-exits
    ctrlc::set_handler(move || {
        if CANCELLED.load(Ordering::Relaxed) {
            eprint!("\x1b[?25h");
            cleanup_tmp_dirs();
            std::process::exit(130);
        }
        CANCELLED.store(true, Ordering::Relaxed);
        eprintln!("\nCancelling after current encodes finish... (Ctrl-C again to force quit)");
    })
    .ok();

    let cfg = config::Config::load(&cli.config)
        .with_context(|| format!("Failed to load config from {:?}", cli.config))?;

    let gpu = gpu::detect_gpu()?;
    eprintln!("GPU: {} ({})", gpu.name, gpu.encoder);

    let has_isomage = iso::isomage_available();

    let files = scanner::scan(&cli.path, &cfg.media_extensions)?;

    if files.is_empty() {
        eprintln!("No media files found in {:?}", cli.path);
        return Ok(());
    }

    // --- Phase 1: Expand disc images into flat work list ---
    let tmp_dir = std::env::temp_dir().join("tdorr_iso_extract");
    let mut expanded: Vec<PathBuf> = Vec::new();
    let mut errors = 0u32;

    for file in &files {
        if iso::is_disc_image(file) {
            if !has_isomage {
                eprintln!(
                    "  skip: {:?} (isomage not found, required for .iso/.img)",
                    file.file_name().unwrap_or_default()
                );
                errors += 1;
                continue;
            }

            eprintln!("  iso: {:?}", file.file_name().unwrap_or_default());

            let inner_files = match iso::list_media_files(file, &cfg.media_extensions) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("  skip: {:?}: {}", file.file_name().unwrap_or_default(), e);
                    errors += 1;
                    continue;
                }
            };

            if inner_files.is_empty() {
                eprintln!("    no media files inside");
                continue;
            }

            eprintln!("    {} media files inside", inner_files.len());

            for inner_path in &inner_files {
                match iso::extract_file(file, inner_path, &tmp_dir) {
                    Ok(p) => expanded.push(p),
                    Err(e) => {
                        eprintln!("    extract failed: {:?}: {}", inner_path, e);
                        errors += 1;
                    }
                }
            }
            continue;
        }

        expanded.push(file.clone());
    }

    // --- Phase 2: Probe all files to partition skip vs. transcode ---
    eprintln!("Scanning {} files...", expanded.len());
    let mut to_transcode: Vec<WorkItem> = Vec::new();
    let mut skipped = 0u32;
    let mut resumed = 0u32;

    for file in &expanded {
        match probe::probe_file(file) {
            Ok(info) => {
                if probe::meets_target(&info, &cfg.target) {
                    skipped += 1;
                } else {
                    let source_size = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);

                    // Resume: check if output already exists and is valid
                    if !cli.overwrite {
                        let out_dir = cli.output_dir.as_deref().or(cfg.output_dir.as_deref());
                        if let Ok(out_path) =
                            transcode::output_path(file, out_dir, &cfg.target.container)
                        {
                            if transcode::output_already_valid(&out_path, file, info.duration_secs)
                            {
                                resumed += 1;
                                continue;
                            }
                        }
                    }

                    to_transcode.push(WorkItem {
                        path: file.clone(),
                        bitrate_kbps: info.bitrate_kbps,
                        duration_secs: info.duration_secs,
                        pix_fmt: info.pix_fmt,
                        source_size,
                    });
                }
            }
            Err(e) => {
                eprintln!(
                    "  skip: {:?}: {}",
                    file.file_name().unwrap_or_default(),
                    e
                );
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
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Ok(());
    }

    let available = gpu::available_sessions(&gpu);
    let jobs = cli.jobs.unwrap_or(available).max(1);

    eprintln!(
        "{} to transcode, {} already HEVC{}, {} jobs",
        to_transcode.len(),
        skipped,
        if resumed > 0 {
            format!(", {} resumed", resumed)
        } else {
            String::new()
        },
        jobs,
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
        let _ = std::fs::remove_dir_all(&tmp_dir);
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
            })
        })
        .collect();
    let completed_lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let completed_units = Arc::new(AtomicU64::new(0));

    // Encoding limiter: discovers actual NVENC session capacity at runtime
    let active_encoders = Arc::new(AtomicU32::new(0));
    let max_encoders = Arc::new(AtomicU32::new(jobs as u32));
    // How many workers are still alive (decremented on retirement)
    let worker_count = Arc::new(AtomicU32::new(jobs as u32));

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
        // Render thread
        {
            let render_slots: Vec<Arc<WorkerSlot>> =
                worker_slots.iter().map(Arc::clone).collect();
            let render_completed = Arc::clone(&completed_lines);
            let render_completed_units = Arc::clone(&completed_units);
            let render_transcoded = Arc::clone(&transcoded);
            let render_errors = Arc::clone(&error_count);
            let render_max = Arc::clone(&max_encoders);

            s.spawn(move || {
                let start = Instant::now();
                let mut prev_viewport = 0usize;

                eprint!("\x1b[?25l");

                loop {
                    {
                        let mut stderr = std::io::stderr().lock();

                        if prev_viewport > 0 {
                            write!(stderr, "\x1b[{}A", prev_viewport).ok();
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
                                if slot.queued.load(Ordering::Relaxed) {
                                    writeln!(
                                        stderr,
                                        "  \u{23f3} queued {}/{} {} ({})",
                                        slot_idx + 1,
                                        max,
                                        name,
                                        size
                                    )
                                    .ok();
                                } else {
                                    let pct = slot.progress.load(Ordering::Relaxed) / 10;
                                    let spd = slot.speed.load(Ordering::Relaxed);
                                    let speed_str = if spd > 0 {
                                        format!(" {}.{}x", spd / 100, (spd % 100) / 10)
                                    } else {
                                        String::new()
                                    };
                                    writeln!(
                                        stderr,
                                        "  \u{25b6} {:>2}%{} {} ({})",
                                        pct, speed_str, name, size
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

                        writeln!(
                            stderr,
                            "  {} {}/{} done  [{:02}:{:02}:{:02}]",
                            progress_bar_str(frac, 40),
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

                    let completed = render_transcoded.load(Ordering::Relaxed);
                    let errs = render_errors.load(Ordering::Relaxed);
                    let cancelled = CANCELLED.load(Ordering::Relaxed);
                    if (completed + errs) as u64 >= file_count || cancelled {
                        let mut stderr = std::io::stderr().lock();
                        if !cancelled && prev_viewport > 0 {
                            write!(stderr, "\x1b[{}A\x1b[J", prev_viewport).ok();
                        }
                        write!(stderr, "\x1b[?25h").ok();
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
            let worker_count = Arc::clone(&worker_count);

            s.spawn(move || 'outer: loop {
                if CANCELLED.load(Ordering::Relaxed) {
                    break;
                }

                let idx = next_idx.fetch_add(1, Ordering::Relaxed) as usize;
                if idx >= to_transcode.len() {
                    break;
                }

                let item = &to_transcode[idx];
                let name = item
                    .path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let short_name = truncate_name(&name, 60);
                let size_str = format_size(item.source_size);

                // Activate slot
                {
                    let mut info = my_slot.info.lock().unwrap();
                    *info = Some((short_name.clone(), size_str));
                }
                my_slot.progress.store(0, Ordering::Relaxed);
                my_slot.speed.store(0, Ordering::Relaxed);

                let output_path = if overwrite {
                    None
                } else {
                    let out_dir = output_dir.as_deref().or(cfg.output_dir.as_deref());
                    match transcode::output_path(&item.path, out_dir, &cfg.target.container) {
                        Ok(p) => Some(p),
                        Err(e) => {
                            completed_lines
                                .lock()
                                .unwrap()
                                .push(format!("  \u{2717} {short_name}: {e}"));
                            error_count.fetch_add(1, Ordering::Relaxed);
                            *my_slot.info.lock().unwrap() = None;
                            completed_units.fetch_add(1000, Ordering::Relaxed);
                            continue;
                        }
                    }
                };

                // Acquire encoding slot + encode with session-limit retry
                let mut session_retries = 0u32;

                let last_err: Option<anyhow::Error> = loop {
                    if CANCELLED.load(Ordering::Relaxed) {
                        break Some(anyhow::anyhow!("cancelled"));
                    }

                    // Wait for an encoding slot
                    if !try_acquire(&active_encoders, &max_encoders) {
                        my_slot.queued.store(true, Ordering::Relaxed);
                        my_slot.progress.store(0, Ordering::Relaxed);
                        my_slot.speed.store(0, Ordering::Relaxed);
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        continue;
                    }
                    my_slot.queued.store(false, Ordering::Relaxed);

                    match transcode::transcode(
                        &item.path,
                        output_path.as_deref(),
                        &cfg.target,
                        &gpu,
                        item.bitrate_kbps,
                        item.duration_secs,
                        &item.pix_fmt,
                        Some(&my_slot.progress),
                        Some(&my_slot.speed),
                    ) {
                        Ok(out_path) => {
                            active_encoders.fetch_sub(1, Ordering::SeqCst);

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
                            completed_lines.lock().unwrap().push(format!(
                                "  \u{2713} {} ({} \u{2192} {}, -{}%)",
                                short_name,
                                format_size(item.source_size),
                                format_size(out_size),
                                saved_pct
                            ));
                            println!("{}", out_path.display());
                            break None;
                        }
                        Err(e) => {
                            active_encoders.fetch_sub(1, Ordering::SeqCst);
                            let err_str = e.to_string();

                            if transcode::is_session_limit_error(&err_str)
                                && session_retries < MAX_SESSION_RETRIES
                            {
                                session_retries += 1;
                                // Lower the discovered max to current active count
                                let active = active_encoders.load(Ordering::SeqCst);
                                lower_max(&max_encoders, active);
                                // Loop back to acquire (will wait if at capacity)
                                my_slot.progress.store(0, Ordering::Relaxed);
                                my_slot.speed.store(0, Ordering::Relaxed);
                                continue;
                            }
                            break Some(e);
                        }
                    }
                };

                if let Some(e) = last_err {
                    completed_lines
                        .lock()
                        .unwrap()
                        .push(format!("  \u{2717} {short_name}: {e}"));
                    error_count.fetch_add(1, Ordering::Relaxed);
                }

                // Deactivate slot
                *my_slot.info.lock().unwrap() = None;
                my_slot.progress.store(0, Ordering::Relaxed);
                my_slot.speed.store(0, Ordering::Relaxed);
                my_slot.queued.store(false, Ordering::Relaxed);
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

    // Drain any completed lines the render thread didn't get to
    {
        let lines = completed_lines.lock().unwrap();
        for line in lines.iter() {
            eprintln!("{}", line);
        }
    }

    // Clean up any leftover .tdorr_tmp_* files
    cleanup_tmp_dirs();

    let was_cancelled = CANCELLED.load(Ordering::Relaxed);

    if was_cancelled {
        let _ = std::fs::remove_dir_all(&tmp_dir);
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
            if let Ok(out_path) =
                transcode::output_path(&item.path, out_dir, &cfg.target.container)
            {
                if out_path.exists() {
                    match transcode::replace_original(&item.path, &out_path, item.duration_secs) {
                        Ok(saved) => {
                            replaced += 1;
                            replace_saved += saved;
                        }
                        Err(e) => {
                            eprintln!(
                                "  \u{2717} replace {:?}: {}",
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

    // Clean up temp extraction dir
    let _ = std::fs::remove_dir_all(&tmp_dir);

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
            "Size: {} \u{2192} {} (saved {}, {:.0}% reduction)",
            format_size(total_input),
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
