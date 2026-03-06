mod config;
mod gpu;
mod iso;
mod probe;
mod scanner;
mod transcode;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "tdorr", about = "Media transcoder - Tdarr that doesn't suck")]
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

    /// Output directory for transcoded files (overrides config)
    #[arg(short, long)]
    output_dir: Option<PathBuf>,
}

fn process_file(
    file: &std::path::Path,
    cli: &Cli,
    cfg: &config::Config,
    gpu: &gpu::GpuInfo,
    skipped: &mut u32,
    transcoded: &mut u32,
    errors: &mut u32,
) -> Result<()> {
    let info = match probe::probe_file(file) {
        Ok(info) => info,
        Err(e) => {
            log::error!("Failed to probe {:?}: {}", file, e);
            *errors += 1;
            return Ok(());
        }
    };

    if probe::meets_target(&info, &cfg.target) {
        log::info!("Skipping {:?} - already meets target", file);
        *skipped += 1;
        return Ok(());
    }

    println!(
        "Transcoding: {:?} ({}, {}x{}, {} kbps)",
        file.file_name().unwrap_or_default(),
        info.codec,
        info.width,
        info.height,
        info.bitrate_kbps,
    );

    if cli.dry_run {
        *transcoded += 1;
        return Ok(());
    }

    let output_path = if cli.overwrite {
        None
    } else {
        let out_dir = cli.output_dir.as_deref().or(cfg.output_dir.as_deref());
        Some(transcode::output_path(file, out_dir, &cfg.target.container)?)
    };

    match transcode::transcode(file, output_path.as_deref(), &cfg.target, gpu) {
        Ok(out) => {
            println!("  -> {:?}", out);
            *transcoded += 1;
        }
        Err(e) => {
            log::error!("Failed to transcode {:?}: {}", file, e);
            *errors += 1;
        }
    }

    Ok(())
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

    let mut skipped = 0;
    let mut transcoded = 0;
    let mut errors = 0;

    // Temp dir for extracted disc image contents (lives for duration of run)
    let tmp_dir = std::env::temp_dir().join("tdorr_iso_extract");

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

            println!("Disc image: {:?}", file.file_name().unwrap_or_default());

            let inner_files = match iso::list_media_files(file, &cfg.media_extensions) {
                Ok(f) => f,
                Err(e) => {
                    log::error!("Failed to list contents of {:?}: {}", file, e);
                    errors += 1;
                    continue;
                }
            };

            if inner_files.is_empty() {
                println!("  No media files found inside disc image");
                skipped += 1;
                continue;
            }

            println!("  Found {} media files inside", inner_files.len());

            for inner_path in &inner_files {
                let extracted = match iso::extract_file(file, inner_path, &tmp_dir) {
                    Ok(p) => p,
                    Err(e) => {
                        log::error!("Failed to extract {:?} from {:?}: {}", inner_path, file, e);
                        errors += 1;
                        continue;
                    }
                };

                match process_file(
                    &extracted, &cli, &cfg, &gpu,
                    &mut skipped, &mut transcoded, &mut errors,
                ) {
                    Ok(()) => {}
                    Err(e) => {
                        log::error!("Error processing {:?}: {}", inner_path, e);
                        errors += 1;
                    }
                }
            }
            continue;
        }

        process_file(
            file, &cli, &cfg, &gpu,
            &mut skipped, &mut transcoded, &mut errors,
        )?;
    }

    // Clean up temp extraction dir
    let _ = std::fs::remove_dir_all(&tmp_dir);

    println!(
        "\nDone: {} transcoded, {} skipped, {} errors (of {} total)",
        transcoded,
        skipped,
        errors,
        files.len()
    );

    Ok(())
}
