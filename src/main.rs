mod config;
mod gpu;
mod iso;
mod probe;
mod scanner;
mod transcode;

use anyhow::{Context, Result};
use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

const MAX_RETRIES: u32 = 3;
const RETRY_DELAY_SECS: u64 = 10;

static CANCELLED: AtomicBool = AtomicBool::new(false);

#[derive(Parser, Debug)]
#[command(name = "tdorr", about = "Media transcoder that I could figure out")]
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

fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1}GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.0}MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.0}KB", bytes as f64 / 1024.0)
    }
}

fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    // Register CTRL-C handler: first press cancels gracefully, second force-exits
    ctrlc::set_handler(move || {
        if CANCELLED.load(Ordering::Relaxed) {
            eprintln!("\nForce quitting...");
            // Clean up tmp files before force exit
            cleanup_tmp_files_best_effort();
            std::process::exit(130);
        }
        CANCELLED.store(true, Ordering::Relaxed);
        eprintln!("\nCancelling after current encodes finish... (Ctrl-C again to force quit)");
    })
    .ok();

    let cfg = config::Config::load(&cli.config)
        .with_context(|| format!("Failed to load config from {:?}", cli.config))?;

    log::info!("tdorr starting with config: {:?}", cli.config);

    let gpu = gpu::detect_gpu()?;
    eprintln!("GPU: {} ({})", gpu.name, gpu.encoder);

    let has_isomage = iso::isomage_available();

    let files = scanner::scan(&cli.path, &cfg.media_extensions)?;

    if files.is_empty() {
        eprintln!("No media files found. Nothing to do.");
        return Ok(());
    }

    // --- Phase 1: Expand disc images into flat work list ---
    let tmp_dir = std::env::temp_dir().join("tdorr_iso_extract");
    let mut expanded: Vec<PathBuf> = Vec::new();
    let mut errors = 0u32;

    for file in &files {
        if iso::is_disc_image(file) {
            if !has_isomage {
                log::error!(
                    "Skipping {:?}: isomage is required for .iso/.img files but not found in PATH",
                    file
                );
                errors += 1;
                continue;
            }

            eprintln!("Disc image: {:?}", file.file_name().unwrap_or_default());

            let inner_files = match iso::list_media_files(file, &cfg.media_extensions) {
                Ok(f) => f,
                Err(e) => {
                    log::error!("Failed to list contents of {:?}: {}", file, e);
                    errors += 1;
                    continue;
                }
            };

            if inner_files.is_empty() {
                eprintln!("  No media files found inside disc image");
                continue;
            }

            eprintln!("  Found {} media files inside", inner_files.len());

            for inner_path in &inner_files {
                match iso::extract_file(file, inner_path, &tmp_dir) {
                    Ok(p) => expanded.push(p),
                    Err(e) => {
                        log::error!("Failed to extract {:?} from {:?}: {}", inner_path, file, e);
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
                                log::info!("Resuming: {:?} already transcoded, skipping", file);
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
                log::error!("Failed to probe {:?}: {}", file, e);
                errors += 1;
            }
        }
    }

    if to_transcode.is_empty() {
        let msg = if resumed > 0 {
            format!(
                "Nothing to do: {} already HEVC, {} already transcoded",
                skipped, resumed
            )
        } else {
            format!(
                "All {} files already meet target. Nothing to do.",
                expanded.len()
            )
        };
        eprintln!("{msg}");
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

    // --- Phase 3: Transcode with multi-progress bars ---
    let mp = MultiProgress::with_draw_target(ProgressDrawTarget::stderr_with_hz(8));

    let worker_style = ProgressStyle::with_template(
        "  {msg:<42} {bar:20.green/dark_gray} {percent:>3}%  {prefix}",
    )
    .unwrap()
    .progress_chars("━╸─");

    let main_style = ProgressStyle::with_template(
        "  {bar:40.cyan/blue} {pos}/{len} transcoded  [{elapsed_precise}]",
    )
    .unwrap()
    .progress_chars("━╸─");

    // Worker bars first (top), then main bar (bottom) — order matters for display
    let worker_bars: Vec<ProgressBar> = (0..jobs)
        .map(|_| {
            let bar = mp.add(ProgressBar::new(1000));
            bar.set_style(worker_style.clone());
            bar.set_message("waiting...");
            bar.enable_steady_tick(std::time::Duration::from_millis(250));
            bar
        })
        .collect();

    let main_bar = mp.add(ProgressBar::new(to_transcode.len() as u64));
    main_bar.set_style(main_style);

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
        for worker_bar in worker_bars.iter().take(jobs) {
            let to_transcode = Arc::clone(&to_transcode);
            let next_idx = Arc::clone(&next_idx);
            let transcoded = Arc::clone(&transcoded);
            let error_count = Arc::clone(&error_count);
            let bytes_saved = Arc::clone(&bytes_saved);
            let bytes_input = Arc::clone(&bytes_input);
            let bytes_output = Arc::clone(&bytes_output);
            let cfg = Arc::clone(&cfg);
            let gpu = Arc::clone(&gpu);
            let bar = worker_bar.clone();
            let main_bar = main_bar.clone();
            let output_dir = output_dir.clone();

            s.spawn(move || loop {
                if CANCELLED.load(Ordering::Relaxed) {
                    bar.set_message("cancelled");
                    bar.abandon();
                    break;
                }

                let idx = next_idx.fetch_add(1, Ordering::Relaxed) as usize;
                if idx >= to_transcode.len() {
                    bar.set_message("done");
                    bar.finish();
                    break;
                }

                let item = &to_transcode[idx];
                let name = item
                    .path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();

                bar.set_message(truncate_name(&name, 42));
                bar.set_position(0);
                bar.set_prefix("");

                let output_path = if overwrite {
                    None
                } else {
                    let out_dir = output_dir.as_deref().or(cfg.output_dir.as_deref());
                    match transcode::output_path(&item.path, out_dir, &cfg.target.container) {
                        Ok(p) => Some(p),
                        Err(e) => {
                            log::error!("Failed to compute output path: {}", e);
                            error_count.fetch_add(1, Ordering::Relaxed);
                            main_bar.inc(1);
                            continue;
                        }
                    }
                };

                // Retry loop for transient NVENC errors
                let mut last_err = None;
                for attempt in 0..=MAX_RETRIES {
                    if CANCELLED.load(Ordering::Relaxed) {
                        last_err = Some(anyhow::anyhow!("cancelled by user"));
                        break;
                    }
                    if attempt > 0 {
                        bar.set_prefix(format!("retry {attempt}/{MAX_RETRIES}"));
                        bar.set_position(0);
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
                        Some(&bar),
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
                            last_err = None;
                            break;
                        }
                        Err(e) => {
                            let err_str = e.to_string();
                            if attempt < MAX_RETRIES && transcode::is_session_limit_error(&err_str)
                            {
                                log::warn!(
                                    "NVENC session limit hit for {:?}, retry {}/{}",
                                    item.path,
                                    attempt + 1,
                                    MAX_RETRIES
                                );
                                last_err = Some(e);
                                continue;
                            }
                            last_err = Some(e);
                            break;
                        }
                    }
                }

                if let Some(e) = last_err {
                    log::error!("Failed to transcode {:?}: {}", item.path, e);
                    error_count.fetch_add(1, Ordering::Relaxed);
                }

                main_bar.inc(1);
            });
        }
    });

    // Clear all progress bars now that workers are done
    for bar in &worker_bars {
        bar.finish_and_clear();
    }
    main_bar.finish_and_clear();

    // Clean up any leftover .tdorr_tmp_* files from this or previous runs
    cleanup_tmp_files(&to_transcode);

    if CANCELLED.load(Ordering::Relaxed) {
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
            if let Ok(out_path) = transcode::output_path(&item.path, out_dir, &cfg.target.container)
            {
                if out_path.exists() {
                    match transcode::replace_original(&item.path, &out_path, item.duration_secs) {
                        Ok(saved) => {
                            replaced += 1;
                            replace_saved += saved;
                        }
                        Err(e) => {
                            log::error!("Failed to replace {:?}: {}", item.path, e);
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
            "Size: {} -> {} (saved {}, {:.0}% reduction)",
            format_size(total_input),
            format_size(total_output),
            format_size(total_saved),
            pct
        );
    }

    Ok(())
}

/// Remove .tdorr_tmp_* files from directories containing work items.
fn cleanup_tmp_files(items: &[WorkItem]) {
    let mut dirs_checked = std::collections::HashSet::new();
    for item in items {
        if let Some(parent) = item.path.parent() {
            if dirs_checked.insert(parent.to_path_buf()) {
                cleanup_tmp_in_dir(parent);
            }
        }
    }
}

fn cleanup_tmp_in_dir(dir: &std::path::Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if name.starts_with(".tdorr_tmp_") {
                    log::info!("Cleaning up tmp file: {:?}", entry.path());
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}

/// Best-effort cleanup for force-quit (second CTRL-C).
/// Scans common locations for tmp files.
fn cleanup_tmp_files_best_effort() {
    // Check current directory and /tmp
    for dir in &[".", "/tmp"] {
        cleanup_tmp_in_dir(std::path::Path::new(dir));
    }
}
