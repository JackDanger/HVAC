mod config;
mod gpu;
mod iso;
mod probe;
mod scanner;
mod transcode;

use anyhow::{Context, Result};
use clap::Parser;
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

struct Counters {
    skipped: AtomicU32,
    transcoded: AtomicU32,
    errors: AtomicU32,
}

fn process_file(
    file: &std::path::Path,
    overwrite: bool,
    dry_run: bool,
    output_dir: Option<&std::path::Path>,
    cfg: &config::Config,
    gpu: &gpu::GpuInfo,
    counters: &Counters,
) {
    let info = match probe::probe_file(file) {
        Ok(info) => info,
        Err(e) => {
            log::error!("Failed to probe {:?}: {}", file, e);
            counters.errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    if probe::meets_target(&info, &cfg.target) {
        log::info!("Skipping {:?} - already meets target", file);
        counters.skipped.fetch_add(1, Ordering::Relaxed);
        return;
    }

    println!(
        "Transcoding: {:?} ({}, {}x{}, {} kbps)",
        file.file_name().unwrap_or_default(),
        info.codec,
        info.width,
        info.height,
        info.bitrate_kbps,
    );

    if dry_run {
        counters.transcoded.fetch_add(1, Ordering::Relaxed);
        return;
    }

    let output_path = if overwrite {
        None
    } else {
        let out_dir = output_dir.or(cfg.output_dir.as_deref());
        match transcode::output_path(file, out_dir, &cfg.target.container) {
            Ok(p) => Some(p),
            Err(e) => {
                log::error!("Failed to compute output path for {:?}: {}", file, e);
                counters.errors.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
    };

    match transcode::transcode(file, output_path.as_deref(), &cfg.target, gpu, info.bitrate_kbps, info.duration_secs) {
        Ok(out) => {
            println!("  -> {:?}", out);
            counters.transcoded.fetch_add(1, Ordering::Relaxed);
        }
        Err(e) => {
            log::error!("Failed to transcode {:?}: {}", file, e);
            counters.errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    let cfg = config::Config::load(&cli.config)
        .with_context(|| format!("Failed to load config from {:?}", cli.config))?;

    log::info!("tdorr starting with config: {:?}", cli.config);

    // Detect GPU - fail fast if none available
    let gpu = gpu::detect_gpu()?;
    println!("GPU detected: {} (encoder: {})", gpu.name, gpu.encoder);

    // Check if isomage is needed and available
    let has_isomage = iso::isomage_available();

    // Scan for media files (including .iso/.img)
    let files = scanner::scan(&cli.path, &cfg.media_extensions)?;
    println!("Found {} media files in {:?}", files.len(), cli.path);

    if files.is_empty() {
        println!("No media files found. Nothing to do.");
        return Ok(());
    }

    let counters = Arc::new(Counters {
        skipped: AtomicU32::new(0),
        transcoded: AtomicU32::new(0),
        errors: AtomicU32::new(0),
    });

    // Temp dir for extracted disc image contents (lives for duration of run)
    let tmp_dir = std::env::temp_dir().join("tdorr_iso_extract");

    // Collect all files to process (expanding disc images)
    let mut work: Vec<PathBuf> = Vec::new();
    for file in &files {
        if iso::is_disc_image(file) {
            if !has_isomage {
                log::error!(
                    "Skipping {:?}: isomage is required for .iso/.img files but not found in PATH",
                    file
                );
                counters.errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            println!("Disc image: {:?}", file.file_name().unwrap_or_default());

            let inner_files = match iso::list_media_files(file, &cfg.media_extensions) {
                Ok(f) => f,
                Err(e) => {
                    log::error!("Failed to list contents of {:?}: {}", file, e);
                    counters.errors.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };

            if inner_files.is_empty() {
                println!("  No media files found inside disc image");
                counters.skipped.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            println!("  Found {} media files inside", inner_files.len());

            for inner_path in &inner_files {
                match iso::extract_file(file, inner_path, &tmp_dir) {
                    Ok(p) => work.push(p),
                    Err(e) => {
                        log::error!("Failed to extract {:?} from {:?}: {}", inner_path, file, e);
                        counters.errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            continue;
        }

        work.push(file.clone());
    }

    let max_sessions = gpu::max_encode_sessions(&gpu);
    let jobs = cli.jobs.unwrap_or(max_sessions).max(1);
    if jobs > 1 {
        println!("Running {} parallel encode jobs (GPU supports up to {})", jobs, max_sessions);
    }

    // Process files with thread pool
    let cfg = Arc::new(cfg);
    let gpu = Arc::new(gpu);
    let pool_size = jobs;
    let work = Arc::new(work);
    let next_idx = Arc::new(AtomicU32::new(0));
    let overwrite = cli.overwrite;
    let dry_run = cli.dry_run;
    let output_dir: Option<PathBuf> = cli.output_dir.clone();

    std::thread::scope(|s| {
        for _ in 0..pool_size {
            let work = Arc::clone(&work);
            let next_idx = Arc::clone(&next_idx);
            let counters = Arc::clone(&counters);
            let cfg = Arc::clone(&cfg);
            let gpu = Arc::clone(&gpu);
            let output_dir = output_dir.clone();
            s.spawn(move || {
                loop {
                    let idx = next_idx.fetch_add(1, Ordering::Relaxed) as usize;
                    if idx >= work.len() {
                        break;
                    }
                    process_file(
                        &work[idx],
                        overwrite,
                        dry_run,
                        output_dir.as_deref(),
                        &cfg,
                        &gpu,
                        &counters,
                    );
                }
            });
        }
    });

    // Clean up temp extraction dir
    let _ = std::fs::remove_dir_all(&tmp_dir);

    let skipped = counters.skipped.load(Ordering::Relaxed);
    let transcoded = counters.transcoded.load(Ordering::Relaxed);
    let errors = counters.errors.load(Ordering::Relaxed);

    println!(
        "\nDone: {} transcoded, {} skipped, {} errors (of {} total)",
        transcoded,
        skipped,
        errors,
        files.len()
    );

    Ok(())
}
