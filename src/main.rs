mod config;
mod gpu;
mod iso;
mod probe;
mod scanner;
mod transcode;

use anyhow::{Context, Result};
use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

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
}

/// File ready to transcode, with pre-probed metadata.
struct WorkItem {
    path: PathBuf,
    bitrate_kbps: u32,
    duration_secs: f64,
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

    for file in &expanded {
        match probe::probe_file(file) {
            Ok(info) => {
                if probe::meets_target(&info, &cfg.target) {
                    skipped += 1;
                } else {
                    to_transcode.push(WorkItem {
                        path: file.clone(),
                        bitrate_kbps: info.bitrate_kbps,
                        duration_secs: info.duration_secs,
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
        eprintln!(
            "All {} files already meet target. Nothing to do.",
            expanded.len()
        );
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Ok(());
    }

    let max_sessions = gpu::max_encode_sessions(&gpu);
    let jobs = cli.jobs.unwrap_or(max_sessions).max(1);

    eprintln!(
        "{} to transcode, {} already HEVC, {} jobs",
        to_transcode.len(),
        skipped,
        jobs,
    );

    if cli.dry_run {
        for item in &to_transcode {
            let name = item
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            eprintln!("  {name} ({} kbps)", item.bitrate_kbps);
        }
        eprintln!("\nDry run: {} would be transcoded", to_transcode.len());
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Ok(());
    }

    // --- Phase 3: Transcode with multi-progress bars ---
    let mp = MultiProgress::new();

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

    // Main bar at the bottom
    let main_bar = mp.add(ProgressBar::new(to_transcode.len() as u64));
    main_bar.set_style(main_style);

    // Worker bars inserted above main bar
    let worker_bars: Vec<ProgressBar> = (0..jobs)
        .map(|_| {
            let bar = mp.insert_before(&main_bar, ProgressBar::new(1000));
            bar.set_style(worker_style.clone());
            bar.set_message("");
            bar
        })
        .collect();

    let transcoded = Arc::new(AtomicU32::new(0));
    let error_count = Arc::new(AtomicU32::new(errors));
    let to_transcode = Arc::new(to_transcode);
    let next_idx = Arc::new(AtomicU32::new(0));
    let cfg = Arc::new(cfg);
    let gpu = Arc::new(gpu);
    let overwrite = cli.overwrite;
    let output_dir = cli.output_dir.clone();

    std::thread::scope(|s| {
        for worker_id in 0..jobs {
            let to_transcode = Arc::clone(&to_transcode);
            let next_idx = Arc::clone(&next_idx);
            let transcoded = Arc::clone(&transcoded);
            let error_count = Arc::clone(&error_count);
            let cfg = Arc::clone(&cfg);
            let gpu = Arc::clone(&gpu);
            let bar = worker_bars[worker_id].clone();
            let main_bar = main_bar.clone();
            let output_dir = output_dir.clone();

            s.spawn(move || {
                loop {
                    let idx = next_idx.fetch_add(1, Ordering::Relaxed) as usize;
                    if idx >= to_transcode.len() {
                        bar.finish_and_clear();
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
                        match transcode::output_path(
                            &item.path,
                            out_dir,
                            &cfg.target.container,
                        ) {
                            Ok(p) => Some(p),
                            Err(e) => {
                                log::error!("Failed to compute output path: {}", e);
                                error_count.fetch_add(1, Ordering::Relaxed);
                                main_bar.inc(1);
                                continue;
                            }
                        }
                    };

                    match transcode::transcode(
                        &item.path,
                        output_path.as_deref(),
                        &cfg.target,
                        &gpu,
                        item.bitrate_kbps,
                        item.duration_secs,
                        Some(&bar),
                    ) {
                        Ok(_) => {
                            transcoded.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            log::error!("Failed to transcode {:?}: {}", item.path, e);
                            error_count.fetch_add(1, Ordering::Relaxed);
                        }
                    }

                    main_bar.inc(1);
                }
            });
        }
    });

    main_bar.finish_and_clear();

    // Clean up temp extraction dir
    let _ = std::fs::remove_dir_all(&tmp_dir);

    let final_transcoded = transcoded.load(Ordering::Relaxed);
    let final_errors = error_count.load(Ordering::Relaxed);

    eprintln!(
        "\nDone: {} transcoded, {} skipped, {} errors",
        final_transcoded, skipped, final_errors
    );

    Ok(())
}
