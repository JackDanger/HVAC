//! hvac entry point. Parses [`Cli`], wires up shared services (LaunchDarkly
//! client, GPU detection, config), then hands off to the pipeline phases.
//!
//! Each pipeline phase lives in [`pipeline`]:
//! [`pipeline::scan`] → [`pipeline::partition`] → [`pipeline::worker`] +
//! [`pipeline::render`], with optional [`pipeline::replace`] after.

mod cli;
mod config;
mod flags;
mod gpu;
mod iso;
mod pipeline;
mod probe;
mod scanner;
mod setup;
mod telemetry;
mod transcode;
mod ui;
mod util;

use anyhow::{Context, Result};
use clap::Parser;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cli::Cli;
use pipeline::worker::WorkerCtx;
use ui::{detect_symbols, embedded_config_banner, max_name_for_cols, terminal_cols};
use util::format_size;

/// RAII guard that flushes LaunchDarkly client + OTel exporter on every
/// `main()` exit path, including early returns and panics. Both methods are
/// idempotent so the explicit flush at end-of-main is a documentation
/// duplicate of this Drop.
struct LdGuard {
    flags: Arc<flags::Flags>,
    telemetry: telemetry::Telemetry,
}

impl Drop for LdGuard {
    fn drop(&mut self) {
        self.flags.close();
        self.telemetry.shutdown();
    }
}

fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    if cli.setup_launchdarkly {
        let api_key = cli
            .ld_api_key
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--setup-launchdarkly requires --ld-api-key <KEY>"))?;
        return setup::run(api_key);
    }

    if cli.dump_config {
        print!("{}", config::EMBEDDED);
        return Ok(());
    }

    // LaunchDarkly client (no-op when --launchdarkly-sdk-key is omitted).
    // SDK key is **CLI-only**: never read from the environment.
    let sdk_key = cli.launchdarkly_sdk_key.as_deref();
    let mut ld_flags = flags::Flags::new(sdk_key);

    // Safety: clap enforces `path` is present unless --dump-config /
    // --setup-launchdarkly is set.
    let path = cli.path.as_deref().unwrap();

    install_sigint_handler();
    let sym = detect_symbols();
    let use_unicode = std::ptr::eq(sym, &ui::UNICODE_SYMBOLS);

    // stdout pipe detection: worker threads write transcoded paths to stdout
    // so callers can pipe them. When stdout is a terminal a println! landing
    // between the render thread's cursor-up and erase moves the cursor to
    // the wrong row, leaving a ghost progress line. Suppress in that case.
    let stdout_is_pipe = unsafe { libc::isatty(libc::STDOUT_FILENO) == 0 };
    let max_name = max_name_for_cols(terminal_cols());

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
    if !gpu.supports_10bit_hevc {
        eprintln!(
            "  note: this GPU's NVENC does not support 10-bit HEVC; \
             10-bit sources will be skipped."
        );
    }
    let gpu_kind = match gpu.kind {
        gpu::GpuKind::Nvidia => "nvidia",
        gpu::GpuKind::Intel => "intel",
        gpu::GpuKind::Apple => "apple",
    };
    ld_flags.set_gpu(&gpu.name, &gpu.encoder, gpu_kind);
    let max_sessions = gpu::max_encode_sessions(&gpu);

    // Now wrap in Arc so the guard and all downstream threads share one client.
    let mut ld_guard = LdGuard {
        flags: Arc::new(ld_flags),
        telemetry: telemetry::Telemetry::new(sdk_key),
    };
    let flags = Arc::clone(&ld_guard.flags);
    flags.track_gpu_detected(&gpu.name, &gpu.encoder, gpu_kind, max_sessions);

    if let Ok(avail) = util::available_disk_space(path) {
        eprintln!("Disk: {} available", format_size(avail));
    }

    // ── Phase 1: scan + expand ────────────────────────────────────────────
    if let Some(fs) = scanner::detect_network_mount(path) {
        eprintln!(
            "Note: {:?} is on a {} mount; ffprobe/scan operations may hang \
             on unresponsive shares. Override probe timeout with --probe-timeout.",
            path, fs
        );
    }
    let files = scanner::scan(path, &cfg.media_extensions)?;
    if files.is_empty() {
        eprintln!("No media files found in {:?}", path);
        return Ok(());
    }

    let scan_result = pipeline::scan::expand(&files);
    let mut errors = scan_result.errors;
    eprintln!("Scanning {} files...", scan_result.items.len());

    let total_bytes: u64 = scan_result
        .items
        .iter()
        .filter_map(|item| std::fs::metadata(&item.file).ok())
        .map(|m| m.len())
        .sum();
    flags.track_scan_completed(scan_result.items.len(), total_bytes);

    // ── Phase 2: probe + filter ────────────────────────────────────────────
    let part = pipeline::partition::partition(&scan_result.items, &cli, &cfg, &gpu);
    let skipped = part.skipped;
    let resumed = part.resumed;
    errors += part.errors;
    let to_transcode = part.to_transcode;

    if to_transcode.is_empty() {
        if resumed > 0 {
            eprintln!(
                "Nothing to do: {} already HEVC, {} already transcoded",
                skipped, resumed
            );
        } else {
            eprintln!("All {} files already meet target.", scan_result.items.len());
        }
        return Ok(());
    }

    let jobs_specified = cli.jobs.is_some();
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

    // Kill-switch: abort before starting Phase 3 if LaunchDarkly says so.
    if !flags.enable_transcoding() {
        eprintln!("Aborted: LaunchDarkly enable-transcoding flag is false.");
        return Ok(());
    }

    // Register work directories for SIGINT cleanup.
    {
        let mut dirs = pipeline::TMP_DIRS.lock().unwrap();
        for item in &to_transcode {
            if let Some(parent) = item.path.parent() {
                let p = parent.to_path_buf();
                if !dirs.contains(&p) {
                    dirs.push(p);
                }
            }
        }
    }

    // ── Phase 3: transcode + render ────────────────────────────────────────
    let transcoded = Arc::new(AtomicU32::new(0));
    // Phase-3 errors only. The render thread terminates when
    // `transcoded + phase3_errors >= file_count`, and `file_count` counts only
    // Phase-3 inputs, so we must not seed this with pre-scan/probe errors —
    // doing so would let the UI exit early while workers are still encoding.
    // Pre-Phase-3 errors are added back into `final_errors` for the summary.
    let phase3_errors = Arc::new(AtomicU32::new(0));
    let bytes_saved = Arc::new(AtomicU64::new(0));
    let bytes_input = Arc::new(AtomicU64::new(0));
    let bytes_output = Arc::new(AtomicU64::new(0));

    let total_transcode_bytes: u64 = to_transcode.iter().map(|i| i.source_size).sum();
    flags.track_run_started(to_transcode.len(), total_transcode_bytes, jobs, auto_ramp);

    let to_transcode = Arc::new(to_transcode);
    run_transcode_phase(
        Arc::clone(&to_transcode),
        &cli,
        cfg.clone(),
        gpu.clone(),
        jobs,
        auto_ramp,
        stdout_is_pipe,
        sym,
        max_name,
        Arc::clone(&transcoded),
        Arc::clone(&phase3_errors),
        Arc::clone(&bytes_saved),
        Arc::clone(&bytes_input),
        Arc::clone(&bytes_output),
        Arc::clone(&flags),
    );

    pipeline::cleanup_tmp_dirs();

    if pipeline::LD_KILL.load(Ordering::Relaxed) {
        eprintln!(
            "\nStopped: {} transcoded before LaunchDarkly kill-switch",
            transcoded.load(Ordering::Relaxed)
        );
    } else if pipeline::CANCELLED.load(Ordering::Relaxed) {
        eprintln!(
            "\nCancelled: {} transcoded before interrupt",
            transcoded.load(Ordering::Relaxed)
        );
        std::process::exit(130);
    }

    if pipeline::LD_KILL.load(Ordering::Relaxed) {
        eprintln!(
            "\nStopped by LaunchDarkly: enable-transcoding=false ({} transcoded)",
            transcoded.load(Ordering::Relaxed)
        );
        // Fall through to track_run_completed + flush before exiting.
    }

    // ── Phase 4 (optional): swap originals with .transcoded.* siblings ────
    if cli.replace && !cli.overwrite() {
        pipeline::replace::run(&to_transcode, &cli, &cfg, sym);
    }
    // Done with the Arc — drop our reference so Phase 4 (which read from it
    // by reference above) doesn't keep it alive past this point.
    drop(to_transcode);

    let final_transcoded = transcoded.load(Ordering::Relaxed);
    // Pre-Phase-3 (scan/probe) errors + Phase-3 (worker) errors → summary total.
    let final_errors = errors + phase3_errors.load(Ordering::Relaxed);
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

    flags.track_run_completed(
        final_transcoded,
        skipped,
        final_errors,
        total_saved,
        total_input,
        total_output,
    );

    // Explicit flush — duplicates the LdGuard's Drop but documents intent.
    flags.close();
    ld_guard.telemetry.shutdown();

    let _ = (resumed,); // silence unused if log paths change

    if final_errors > 0 {
        std::process::exit(1);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_transcode_phase(
    to_transcode: Arc<Vec<pipeline::WorkItem>>,
    cli: &Cli,
    cfg: config::Config,
    gpu: gpu::GpuInfo,
    jobs: usize,
    auto_ramp: bool,
    stdout_is_pipe: bool,
    sym: &'static ui::Symbols,
    max_name: usize,
    transcoded: Arc<AtomicU32>,
    phase3_errors: Arc<AtomicU32>,
    bytes_saved: Arc<AtomicU64>,
    bytes_input: Arc<AtomicU64>,
    bytes_output: Arc<AtomicU64>,
    flags: Arc<flags::Flags>,
) {
    let total_units = to_transcode.len() as u64 * 1000;
    let file_count = to_transcode.len() as u64;

    let worker_slots: Vec<Arc<pipeline::WorkerSlot>> = (0..jobs)
        .map(|_| Arc::new(pipeline::WorkerSlot::new()))
        .collect();
    let completed_lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let completed_units = Arc::new(AtomicU64::new(0));

    let active_encoders = Arc::new(AtomicU32::new(0));
    let initial_max = if auto_ramp { 1u32 } else { jobs as u32 };
    let max_encoders = Arc::new(AtomicU32::new(initial_max));
    let ramping = Arc::new(AtomicBool::new(auto_ramp));
    let worker_count = Arc::new(AtomicU32::new(jobs as u32));
    let session_limit_hits = Arc::new(AtomicU32::new(0));
    let session_limit_frozen = Arc::new(AtomicBool::new(false));
    let min_observed_max = Arc::new(AtomicU32::new(u32::MAX));
    let disk_reserved = Arc::new(AtomicU64::new(0));

    let next_idx = Arc::new(AtomicU32::new(0));
    let cfg = Arc::new(cfg);
    let gpu = Arc::new(gpu);
    let output_dir = cli.output_dir.clone();
    let overwrite = cli.overwrite();

    std::thread::scope(|s| {
        // Render thread (drives auto-ramp too).
        {
            let render_ctx = pipeline::render::RenderCtx {
                slots: worker_slots.iter().map(Arc::clone).collect(),
                completed_lines: Arc::clone(&completed_lines),
                completed_units: Arc::clone(&completed_units),
                transcoded: Arc::clone(&transcoded),
                errors: Arc::clone(&phase3_errors),
                max_encoders: Arc::clone(&max_encoders),
                ramping: Arc::clone(&ramping),
                session_limit_frozen: Arc::clone(&session_limit_frozen),
                total_units,
                file_count,
                sym,
                flags: Arc::clone(&flags),
            };
            s.spawn(move || pipeline::render::run_render(render_ctx));
        }

        // Worker threads.
        for slot in &worker_slots {
            let ctx = WorkerCtx {
                to_transcode: Arc::clone(&to_transcode),
                next_idx: Arc::clone(&next_idx),
                cfg: Arc::clone(&cfg),
                gpu: Arc::clone(&gpu),
                output_dir: output_dir.clone(),
                overwrite,
                auto_ramp,
                jobs,
                stdout_is_pipe,
                sym,
                max_name,
                transcoded: Arc::clone(&transcoded),
                error_count: Arc::clone(&phase3_errors),
                bytes_saved: Arc::clone(&bytes_saved),
                bytes_input: Arc::clone(&bytes_input),
                bytes_output: Arc::clone(&bytes_output),
                completed_units: Arc::clone(&completed_units),
                completed_lines: Arc::clone(&completed_lines),
                active_encoders: Arc::clone(&active_encoders),
                max_encoders: Arc::clone(&max_encoders),
                worker_count: Arc::clone(&worker_count),
                ramping: Arc::clone(&ramping),
                session_limit_hits: Arc::clone(&session_limit_hits),
                session_limit_frozen: Arc::clone(&session_limit_frozen),
                min_observed_max: Arc::clone(&min_observed_max),
                disk_reserved: Arc::clone(&disk_reserved),
                flags: Arc::clone(&flags),
            };
            let my_slot = Arc::clone(slot);
            s.spawn(move || pipeline::worker::run_worker(ctx, my_slot));
        }
    });

    // Drain any completed lines the render thread didn't get to (skip on cancel).
    if !pipeline::CANCELLED.load(Ordering::Relaxed) {
        let lines = completed_lines.lock().unwrap();
        for line in lines.iter() {
            eprintln!("{}", line);
        }
    }
}

/// First Ctrl-C cancels gracefully; second Ctrl-C force-exits.
extern "C" fn sigint_handler(_: libc::c_int) {
    const RESET: &[u8] = b"\x1b[0m\r\n";
    const MSG: &[u8] = b"Cancelling after current encodes finish... (Ctrl-C again to force quit)\n";
    // SAFETY: write(2) is async-signal-safe.
    unsafe { libc::write(2, RESET.as_ptr() as *const libc::c_void, RESET.len()) };
    if pipeline::CANCELLED.load(Ordering::Relaxed) {
        pipeline::cleanup_tmp_dirs();
        // SAFETY: _exit is async-signal-safe.
        unsafe { libc::_exit(130) };
    }
    pipeline::CANCELLED.store(true, Ordering::Relaxed);
    unsafe { libc::write(2, MSG.as_ptr() as *const libc::c_void, MSG.len()) };
}

fn install_sigint_handler() {
    // SAFETY: sigint_handler only touches a static AtomicBool and
    // async-signal-safe libc functions.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = sigint_handler as *const () as libc::sighandler_t;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_RESTART;
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
    }
}
