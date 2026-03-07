mod config;
mod gpu;
mod iso;
mod probe;
mod scanner;
mod transcode;
mod util;

use anyhow::{Context, Result};
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use util::format_size;

const MAX_RETRIES: u32 = 3;
const RETRY_DELAY_SECS: u64 = 10;

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

fn truncate_name(name: &str, max_len: usize) -> String {
    if name.chars().count() <= max_len {
        name.to_string()
    } else {
        let truncated: String = name.chars().take(max_len - 1).collect();
        format!("{truncated}…")
    }
}

fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    // Register CTRL-C handler: first press cancels gracefully, second force-exits
    ctrlc::set_handler(move || {
        if CANCELLED.load(Ordering::Relaxed) {
            // Second press: force-quit with best-effort cleanup
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

    // Account for other running NVENC sessions
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

    // --- Phase 3: Transcode ---
    // Bar tracks total progress in milli-files (0-1000 per file) for smooth movement
    let total_units = to_transcode.len() as u64 * 1000;
    let bar = ProgressBar::new(total_units);
    bar.set_style(
        ProgressStyle::with_template(
            "  {bar:40.cyan/blue} {msg}  [{elapsed_precise}]",
        )
        .unwrap()
        .progress_chars("━╸─"),
    );
    let completed_units = Arc::new(AtomicU64::new(0));
    let active_count = Arc::new(AtomicU32::new(0));
    // Per-worker progress atomics (0-1000 each)
    let worker_progress: Vec<Arc<AtomicU64>> =
        (0..jobs).map(|_| Arc::new(AtomicU64::new(0))).collect();

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
        // Render thread: updates bar position from worker progress atomics
        {
            let bar = bar.clone();
            let completed_units = Arc::clone(&completed_units);
            let worker_progress: Vec<Arc<AtomicU64>> =
                worker_progress.iter().map(Arc::clone).collect();
            let file_count = to_transcode.len() as u64;
            let transcoded = Arc::clone(&transcoded);
            let error_count_r = Arc::clone(&error_count);
            s.spawn(move || loop {
                let done = completed_units.load(Ordering::Relaxed);
                let active: u64 = worker_progress.iter().map(|w| w.load(Ordering::Relaxed)).sum();
                bar.set_position((done + active).min(total_units));

                let completed = transcoded.load(Ordering::Relaxed);
                let errors = error_count_r.load(Ordering::Relaxed);
                let finished = completed + errors;

                // Show per-worker encode percentages so user can see activity
                let pcts: Vec<String> = worker_progress
                    .iter()
                    .map(|w| w.load(Ordering::Relaxed))
                    .filter(|&p| p > 0)
                    .map(|p| format!("{}%", p / 10))
                    .collect();
                let detail = if pcts.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", pcts.join(" "))
                };
                bar.set_message(format!("{finished}/{file_count} done{detail}"));

                if finished as u64 >= file_count || CANCELLED.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            });
        }

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
            let bar = bar.clone();
            let output_dir = output_dir.clone();
            let active_count = Arc::clone(&active_count);
            let completed_units = Arc::clone(&completed_units);
            let my_progress = Arc::clone(&worker_progress[worker_id]);

            s.spawn(move || loop {
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

                active_count.fetch_add(1, Ordering::Relaxed);
                my_progress.store(0, Ordering::Relaxed);
                bar.println(format!("  ▶ {short_name} ({})", format_size(item.source_size)));

                let output_path = if overwrite {
                    None
                } else {
                    let out_dir = output_dir.as_deref().or(cfg.output_dir.as_deref());
                    match transcode::output_path(&item.path, out_dir, &cfg.target.container) {
                        Ok(p) => Some(p),
                        Err(e) => {
                            bar.println(format!("  ✗ {short_name}: {e}"));
                            error_count.fetch_add(1, Ordering::Relaxed);
                            my_progress.store(0, Ordering::Relaxed);
                            completed_units.fetch_add(1000, Ordering::Relaxed);
                            active_count.fetch_sub(1, Ordering::Relaxed);
                            continue;
                        }
                    }
                };

                // Retry loop for transient NVENC errors
                let mut last_err = None;
                for attempt in 0..=MAX_RETRIES {
                    if CANCELLED.load(Ordering::Relaxed) {
                        last_err = Some(anyhow::anyhow!("cancelled"));
                        break;
                    }
                    if attempt > 0 {
                        bar.println(format!(
                            "  ↻ {short_name} retry {attempt}/{MAX_RETRIES}"
                        ));
                        std::thread::sleep(std::time::Duration::from_secs(
                            RETRY_DELAY_SECS * attempt as u64,
                        ));
                    }

                    match transcode::transcode(
                        &item.path,
                        output_path.as_deref(),
                        &cfg.target,
                        &gpu,
                        item.bitrate_kbps,
                        item.duration_secs,
                        &item.pix_fmt,
                        Some(&my_progress),
                    ) {
                        Ok(out_path) => {
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
                            bar.println(format!(
                                "  ✓ {short_name} ({} → {}, -{}%)",
                                format_size(item.source_size),
                                format_size(out_size),
                                saved_pct
                            ));
                            // stdout: completed output path for pipeline use
                            println!("{}", out_path.display());
                            last_err = None;
                            break;
                        }
                        Err(e) => {
                            let err_str = e.to_string();
                            if attempt < MAX_RETRIES && transcode::is_session_limit_error(&err_str)
                            {
                                last_err = Some(e);
                                continue;
                            }
                            last_err = Some(e);
                            break;
                        }
                    }
                }

                if let Some(e) = last_err {
                    bar.println(format!("  ✗ {short_name}: {e}"));
                    error_count.fetch_add(1, Ordering::Relaxed);
                }

                my_progress.store(0, Ordering::Relaxed);
                completed_units.fetch_add(1000, Ordering::Relaxed);
                active_count.fetch_sub(1, Ordering::Relaxed);
            });
        }
    });

    bar.finish_and_clear();

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
                                "  ✗ replace {:?}: {}",
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
            "Size: {} → {} (saved {}, {:.0}% reduction)",
            format_size(total_input),
            format_size(total_output),
            format_size(total_saved),
            pct
        );
    }

    // Exit non-zero if any files failed
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
