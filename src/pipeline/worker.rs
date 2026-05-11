//! Encode worker thread + retry state machine.
//!
//! Each worker pulls the next [`WorkItem`] off the shared queue, computes the
//! output path, and runs the encode. On failure the worker walks through a
//! fixed sequence of recovery tiers (see [`RetryDecision`]) before giving up
//! and recording an error. On success it updates the byte counters and pushes
//! a completed line onto the shared queue for the render thread.
//!
//! Atomics and Arcs holding the shared state are bundled in [`WorkerCtx`] so
//! the spawn-loop in `main` doesn't grow tentacles.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::Config;
use crate::flags::Flags;
use crate::gpu::GpuInfo;
use crate::transcode;
use crate::ui::{truncate_name, Symbols};
use crate::util::{self, format_size};

use super::{WorkItem, WorkerSlot, CANCELLED, TMP_DIRS};

/// Maximum times a single file may retry on session-limit / disk-space
/// errors. Each retry waits for the offending pressure to clear.
pub const MAX_SESSION_RETRIES: u32 = 5;

/// After this many cumulative NVENC session-limit hits across the entire run,
/// permanently freeze `max_encoders` at the lowest observed working value and
/// disable auto-ramping. This stops the climb-fail-retry-climb cycle on
/// consumer GeForce cards (3-session NVENC cap).
pub const MAX_SESSION_LIMIT_BEFORE_FREEZE: u32 = 3;

/// Returns true once the cumulative session-limit hit count reaches the
/// freeze threshold. `hits` is the post-increment count.
pub fn should_freeze(hits: u32) -> bool {
    hits >= MAX_SESSION_LIMIT_BEFORE_FREEZE
}

/// Disk-space safety margin: never start an encode that would leave less
/// than this much free on the output filesystem.
pub const DISK_MARGIN: u64 = 2 * 1024 * 1024 * 1024;

/// What the retry state machine wants to do after an ffmpeg failure.
///
/// The tier order matters: cheap, deterministic fixes first (drop a codec
/// to a re-encode), then resource-pressure waits (disk space), then the
/// nuclear option (drop subs entirely). Session-limit retries can apply
/// at any tier and are independent of the codec fallbacks.
///
/// The classifier is pure / testable — see [`classify_failure`].
#[derive(Debug, PartialEq, Eq)]
pub enum RetryDecision {
    /// Re-encode audio instead of `-c:a copy`. Used when the source's audio
    /// codec isn't muxable as-is into the chosen container (e.g. pcm_dvd → MKV).
    ReencodeAudio,
    /// Convert subtitles to a container-appropriate text codec
    /// (srt for MKV, mov_text for MP4) before dropping them.
    ReencodeSubtitles,
    /// Drop subtitle streams entirely. Final fallback for incompatible
    /// bitmap subs.
    SkipSubtitles,
    /// NVENC session-limit hit. Lower concurrency, possibly freeze.
    SessionLimit,
    /// Disk-space exhaustion. Wait for other encodes to free space.
    DiskSpace,
    /// Nothing we know how to recover from; surface the error to the user.
    Bail,
}

/// Worker-local retry state. Each tier fires at most once per file (except
/// session-limit / disk-space, which can retry up to `MAX_SESSION_RETRIES`).
#[derive(Default, Debug)]
pub struct RetryState {
    pub session_retries: u32,
    pub disk_space_retries: u32,
    pub force_reencode_audio: bool,
    pub subtitle_reencode_attempt: Option<&'static str>,
    pub skip_subs: bool,
}

/// Classify an ffmpeg failure into a [`RetryDecision`].
///
/// Pure function over the error message — predicates from
/// [`crate::transcode`] do the actual string matching. Centralised here so
/// the worker loop doesn't have to spell out the tier order inline.
pub fn classify_failure(err_str: &str, state: &RetryState) -> RetryDecision {
    if transcode::is_disk_space_error(err_str) && state.disk_space_retries < MAX_SESSION_RETRIES {
        return RetryDecision::DiskSpace;
    }
    if transcode::is_audio_copy_error(err_str) && !state.force_reencode_audio {
        return RetryDecision::ReencodeAudio;
    }
    // "Nothing was written" can also indicate an incompatible subtitle codec
    // — try re-encoding subs first, then drop entirely.
    let nothing_written = err_str.contains("Nothing was written into output file");
    let looks_subtitle = transcode::is_subtitle_error(err_str) || nothing_written;
    if looks_subtitle && state.subtitle_reencode_attempt.is_none() && !state.skip_subs {
        return RetryDecision::ReencodeSubtitles;
    }
    if looks_subtitle && !state.skip_subs {
        return RetryDecision::SkipSubtitles;
    }
    if transcode::is_session_limit_error(err_str) && state.session_retries < MAX_SESSION_RETRIES {
        return RetryDecision::SessionLimit;
    }
    RetryDecision::Bail
}

/// Compute the source path used as the output-file name stem.
///
/// Shared between the pre-transcode resume check and the worker thread.
/// Both must produce identical results; if they diverge the resume check
/// looks for the wrong output file and re-transcodes on every run.
///
/// * Any ISO (single or multi-file) → ISO path
/// * Regular file → file path itself
///
/// `title_suffix`, when set, is appended to the file stem so multi-title discs
/// emit distinct outputs per title (e.g. `Disc.title01.transcoded.mkv`).
pub fn output_stem_for_item(file: &Path, title_suffix: Option<&str>) -> PathBuf {
    match title_suffix {
        Some(suffix) => {
            let stem = file
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let ext = file
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_default();
            let new_name = if ext.is_empty() {
                format!("{}.{}", stem, suffix)
            } else {
                format!("{}.{}.{}", stem, suffix, ext)
            };
            match file.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent.join(new_name),
                _ => PathBuf::from(new_name),
            }
        }
        None => file.to_path_buf(),
    }
}

/// Try to acquire an encoding slot. Returns true if acquired.
/// Uses compare_exchange to atomically check `active < max` and increment.
pub fn try_acquire(active: &AtomicU32, max: &AtomicU32) -> bool {
    loop {
        let a = active.load(Ordering::SeqCst);
        let m = max.load(Ordering::SeqCst);
        if a >= m {
            return false;
        }
        if active
            .compare_exchange_weak(a, a + 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return true;
        }
    }
}

/// Lower `max` to at most `new_max`. Never raises.
pub fn lower_max(max: &AtomicU32, new_max: u32) {
    loop {
        let current = max.load(Ordering::SeqCst);
        if current <= new_max {
            return;
        }
        if max
            .compare_exchange_weak(current, new_max, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return;
        }
    }
}

/// Everything a worker thread needs to do its job. Arcs are cheap to clone
/// per-worker; bundling them prevents the spawn loop from being a wall of
/// `Arc::clone(&...)` lines.
pub struct WorkerCtx {
    pub to_transcode: Arc<Vec<WorkItem>>,
    pub next_idx: Arc<AtomicU32>,
    pub cfg: Arc<Config>,
    pub gpu: Arc<GpuInfo>,
    pub output_dir: Option<PathBuf>,
    pub overwrite: bool,
    pub auto_ramp: bool,
    pub jobs: usize,
    pub stdout_is_pipe: bool,
    pub sym: &'static Symbols,
    pub max_name: usize,
    // Counters / display state
    pub transcoded: Arc<AtomicU32>,
    pub error_count: Arc<AtomicU32>,
    pub bytes_saved: Arc<AtomicU64>,
    pub bytes_input: Arc<AtomicU64>,
    pub bytes_output: Arc<AtomicU64>,
    pub completed_units: Arc<AtomicU64>,
    pub completed_lines: Arc<Mutex<Vec<String>>>,
    // Concurrency limiter
    pub active_encoders: Arc<AtomicU32>,
    pub max_encoders: Arc<AtomicU32>,
    pub worker_count: Arc<AtomicU32>,
    pub ramping: Arc<AtomicBool>,
    pub session_limit_hits: Arc<AtomicU32>,
    pub session_limit_frozen: Arc<AtomicBool>,
    pub min_observed_max: Arc<AtomicU32>,
    pub disk_reserved: Arc<AtomicU64>,
    pub flags: Arc<Flags>,
}

/// Run one worker iteration for the slot bound to `my_slot`. Exits when the
/// queue drains, the run is cancelled, or this worker is asked to retire.
pub fn run_worker(ctx: WorkerCtx, my_slot: Arc<WorkerSlot>) {
    'outer: loop {
        if CANCELLED.load(Ordering::Relaxed) {
            return;
        }

        // Kill-switch: stop picking up new files.
        if !ctx.flags.enable_transcoding() {
            log::info!("LaunchDarkly enable-transcoding=false: worker stopping");
            CANCELLED.store(true, Ordering::Relaxed);
            return;
        }

        // Pause: spin-wait while paused; in-flight encodes finish first.
        let mut was_paused = false;
        while ctx.flags.pause_transcoding() && !CANCELLED.load(Ordering::Relaxed) {
            if !was_paused {
                let active = ctx.active_encoders.load(Ordering::Relaxed) as usize;
                ctx.flags.track_transcoding_paused(active);
                was_paused = true;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        if was_paused {
            let active = ctx.active_encoders.load(Ordering::Relaxed) as usize;
            ctx.flags.track_transcoding_resumed(active);
        }

        let idx = ctx.next_idx.fetch_add(1, Ordering::Relaxed) as usize;
        if idx >= ctx.to_transcode.len() {
            return;
        }
        let item = &ctx.to_transcode[idx];

        // Build the display name. ISOs get the ISO filename + an optional
        // [titleNN] tag + the inner file count when multi-file.
        let name = display_name_for_item(item);
        let short_name = truncate_name(&name, ctx.max_name, ctx.sym);
        let size_str = format_size(item.source_size);

        // Fixed-jobs mode: become visible immediately (so users see "queued"
        // for excess workers). Auto-ramp: stay invisible until we acquire a
        // slot so the viewport doesn't show ghost rows.
        if !ctx.auto_ramp {
            let mut info = my_slot.info.lock().unwrap();
            *info = Some((short_name.clone(), size_str.clone()));
            my_slot.progress.store(0, Ordering::Relaxed);
            my_slot.speed.store(0, Ordering::Relaxed);
        }

        // Compute the output path. In `--overwrite` mode for non-ISO sources
        // we transcode in-place (output = None); otherwise we write a sibling
        // `.transcoded.<container>` (or into output_dir).
        let output_path = match resolve_output_path(item, &ctx) {
            Ok(p) => p,
            Err(e) => {
                ctx.completed_lines
                    .lock()
                    .unwrap()
                    .push(format!("  {} {short_name}: {e}", ctx.sym.cross));
                ctx.error_count.fetch_add(1, Ordering::Relaxed);
                my_slot.clear();
                ctx.completed_units.fetch_add(1000, Ordering::Relaxed);
                continue 'outer;
            }
        };

        // Register the temp dir for SIGINT-twice cleanup. We use the source's
        // parent for in-place (None output_path), else the output's parent.
        register_tmp_dir(item, output_path.as_deref());

        // Encode size estimate for disk reservation: conservative 50% of source.
        let disk_estimate = (item.source_size / 2).max(100 * 1024 * 1024);

        let mut state = RetryState::default();
        let last_err = encode_with_retry(
            item,
            &output_path,
            disk_estimate,
            &mut state,
            &ctx,
            &my_slot,
            &short_name,
            &size_str,
        );

        my_slot.clear();
        if let Some(e) = last_err {
            ctx.completed_lines
                .lock()
                .unwrap()
                .push(format!("  {} {short_name}: {e}", ctx.sym.cross));
            ctx.error_count.fetch_add(1, Ordering::Relaxed);
        }
        ctx.completed_units.fetch_add(1000, Ordering::Relaxed);

        // Retire excess workers: if we discovered a lower capacity, workers
        // beyond that count exit after finishing their file.
        let max = ctx.max_encoders.load(Ordering::SeqCst);
        if max < ctx.jobs as u32 {
            let wc = ctx.worker_count.load(Ordering::SeqCst);
            if wc > max
                && ctx
                    .worker_count
                    .compare_exchange(wc, wc - 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                break 'outer;
            }
        }
    }
}

/// Resolve the output path for a work item. Returns `Ok(None)` for in-place
/// (overwrite, non-ISO) and `Ok(Some(path))` for everything else.
fn resolve_output_path(item: &WorkItem, ctx: &WorkerCtx) -> anyhow::Result<Option<PathBuf>> {
    if ctx.overwrite && item.iso_path.is_none() {
        return Ok(None);
    }
    let out_dir = ctx.output_dir.as_deref().or(ctx.cfg.output_dir.as_deref());
    let source_for_output = output_stem_for_item(&item.path, item.title_suffix.as_deref());
    let path = transcode::output_path(&source_for_output, out_dir, &ctx.cfg.target.container)?;
    Ok(Some(path))
}

/// Format the per-worker display name for `item`, including an optional
/// `[titleNN]` tag for multi-title discs.
fn display_name_for_item(item: &WorkItem) -> String {
    let iso_name = || item.path.file_name().unwrap_or_default().to_string_lossy();
    let title = item.title_suffix.as_deref();
    match (&item.inner_paths, &item.inner_path) {
        (Some(paths), _) => match title {
            Some(s) => format!("{} [{}] ({} files)", iso_name(), s, paths.len()),
            None => format!("{} ({} files)", iso_name(), paths.len()),
        },
        (None, Some(inner)) => {
            let inner_name = Path::new(inner)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            match title {
                Some(s) => format!("{} [{}]:{}", iso_name(), s, inner_name),
                None => format!("{}:{}", iso_name(), inner_name),
            }
        }
        (None, None) => item
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    }
}

/// Register a directory where `.hvac_tmp_*` files will land during this
/// encode, so the SIGINT-twice cleanup can sweep it.
fn register_tmp_dir(item: &WorkItem, output_path: Option<&Path>) {
    let parent = output_path
        .and_then(|p| p.parent())
        .or_else(|| item.path.parent());
    if let Some(dir) = parent {
        if let Ok(mut dirs) = TMP_DIRS.lock() {
            let dir_buf = dir.to_path_buf();
            if !dirs.contains(&dir_buf) {
                dirs.push(dir_buf);
            }
        }
    }
}

/// Drive one file through the encode + retry tiers, returning the final error
/// (if any) for the caller to surface to the user.
#[allow(clippy::too_many_arguments)]
fn encode_with_retry(
    item: &WorkItem,
    output_path: &Option<PathBuf>,
    disk_estimate: u64,
    state: &mut RetryState,
    ctx: &WorkerCtx,
    my_slot: &Arc<WorkerSlot>,
    short_name: &str,
    size_str: &str,
) -> Option<anyhow::Error> {
    loop {
        if CANCELLED.load(Ordering::Relaxed) {
            return None;
        }

        if !wait_for_disk(
            item,
            output_path.as_deref(),
            disk_estimate,
            ctx,
            my_slot,
            short_name,
            size_str,
        ) {
            continue;
        }

        // Wait for an encoding slot. In fixed mode show "queued"; in auto we
        // stay invisible until we have a slot.
        if !try_acquire(&ctx.active_encoders, &ctx.max_encoders) {
            if !ctx.auto_ramp {
                if !my_slot.queued.swap(true, Ordering::Relaxed) {
                    // Just became queued — track it once.
                    ctx.flags.track_transcode_queued(
                        short_name,
                        0,
                        ctx.max_encoders.load(Ordering::Relaxed),
                    );
                }
                my_slot.progress.store(0, Ordering::Relaxed);
                my_slot.speed.store(0, Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        my_slot.queued.store(false, Ordering::Relaxed);
        ctx.disk_reserved.fetch_add(disk_estimate, Ordering::SeqCst);

        if ctx.auto_ramp {
            let mut info = my_slot.info.lock().unwrap();
            *info = Some((short_name.to_string(), size_str.to_string()));
            my_slot.progress.store(0, Ordering::Relaxed);
            my_slot.speed.store(0, Ordering::Relaxed);
        }

        ctx.flags.track_transcode_started(
            short_name,
            item.bitrate_kbps,
            item.duration_secs,
            item.source_size,
            &item.pix_fmt,
        );

        let encode_result = run_one_encode(item, output_path.as_deref(), state, ctx, my_slot);

        match encode_result {
            Ok(out_path) => {
                ctx.active_encoders.fetch_sub(1, Ordering::SeqCst);
                ctx.disk_reserved.fetch_sub(disk_estimate, Ordering::SeqCst);
                record_success(item, &out_path, ctx, my_slot, short_name);
                return None;
            }
            Err(e) => {
                ctx.active_encoders.fetch_sub(1, Ordering::SeqCst);
                ctx.disk_reserved.fetch_sub(disk_estimate, Ordering::SeqCst);

                let err_str = e.to_string();
                let decision = classify_failure(&err_str, state);
                if !apply_retry(
                    decision, &err_str, state, ctx, my_slot, short_name, size_str,
                ) {
                    ctx.flags.track_transcode_failed(short_name, &err_str);
                    return Some(e);
                }
            }
        }
    }
}

/// Check available disk space; if too tight, mark the slot as waiting and
/// return `false` so the caller loops again. Returns `true` when there's
/// enough space (or we couldn't determine — we proceed optimistically).
fn wait_for_disk(
    item: &WorkItem,
    output_path: Option<&Path>,
    disk_estimate: u64,
    ctx: &WorkerCtx,
    my_slot: &Arc<WorkerSlot>,
    short_name: &str,
    size_str: &str,
) -> bool {
    let check_dir = output_path
        .and_then(|p| p.parent())
        .unwrap_or_else(|| item.path.parent().unwrap_or(Path::new("/")));

    let has_disk = if let Ok(avail) = util::available_disk_space(check_dir) {
        let reserved = ctx.disk_reserved.load(Ordering::SeqCst);
        let effective = avail.saturating_sub(reserved);
        reserved == 0 || effective >= disk_estimate + DISK_MARGIN
    } else {
        true
    };

    if !has_disk {
        if !my_slot.disk_wait.swap(true, Ordering::Relaxed) {
            // First time hitting disk pressure for this file — track it once.
            let avail_gb = if let Ok(a) = util::available_disk_space(check_dir) {
                a as f64 / (1024 * 1024 * 1024) as f64
            } else {
                0.0
            };
            ctx.flags.track_disk_wait(short_name, avail_gb);
        }
        if !ctx.auto_ramp {
            let mut info = my_slot.info.lock().unwrap();
            *info = Some((short_name.to_string(), size_str.to_string()));
        }
        std::thread::sleep(Duration::from_millis(2000));
        return false;
    }
    my_slot.disk_wait.store(false, Ordering::Relaxed);
    true
}

/// Run exactly one transcode attempt (no retries here — that's the outer loop).
fn run_one_encode(
    item: &WorkItem,
    output_path: Option<&Path>,
    state: &RetryState,
    ctx: &WorkerCtx,
    my_slot: &Arc<WorkerSlot>,
) -> anyhow::Result<PathBuf> {
    if let Some(ref iso) = item.iso_path {
        let out = output_path.expect("ISO entries always need an output path");
        let paths = item
            .inner_paths
            .clone()
            .or_else(|| item.inner_path.as_ref().map(|p| vec![p.clone()]))
            .unwrap_or_default();
        transcode::transcode_iso(
            iso,
            &paths,
            out,
            &ctx.cfg.target,
            &ctx.gpu,
            item.bitrate_kbps,
            item.duration_secs,
            &item.pix_fmt,
            &item.color,
            Some(&my_slot.progress),
            Some(&my_slot.speed),
            state.skip_subs,
            state.force_reencode_audio,
            state.subtitle_reencode_attempt,
        )
    } else {
        transcode::transcode(
            &item.path,
            output_path,
            &ctx.cfg.target,
            &ctx.gpu,
            item.bitrate_kbps,
            item.duration_secs,
            &item.pix_fmt,
            &item.color,
            Some(&my_slot.progress),
            Some(&my_slot.speed),
            state.skip_subs,
            state.force_reencode_audio,
            state.subtitle_reencode_attempt,
        )
    }
}

/// Apply a [`RetryDecision`]. Returns `true` to keep retrying, `false` to give up.
fn apply_retry(
    decision: RetryDecision,
    err_str: &str,
    state: &mut RetryState,
    ctx: &WorkerCtx,
    my_slot: &Arc<WorkerSlot>,
    short_name: &str,
    size_str: &str,
) -> bool {
    let _ = err_str;
    match decision {
        RetryDecision::DiskSpace => {
            state.disk_space_retries += 1;
            my_slot.disk_wait.store(true, Ordering::Relaxed);
            *my_slot.info.lock().unwrap() = Some((short_name.to_string(), size_str.to_string()));
            my_slot.progress.store(0, Ordering::Relaxed);
            my_slot.speed.store(0, Ordering::Relaxed);
            std::thread::sleep(Duration::from_secs(5));
            my_slot.disk_wait.store(false, Ordering::Relaxed);
            true
        }
        RetryDecision::ReencodeAudio => {
            state.force_reencode_audio = true;
            log::info!(
                "{}: retrying with audio re-encode (source codec not muxable as copy)",
                short_name
            );
            my_slot.progress.store(0, Ordering::Relaxed);
            my_slot.speed.store(0, Ordering::Relaxed);
            true
        }
        RetryDecision::ReencodeSubtitles => {
            let codec = transcode::subtitle_reencode_fallback(&ctx.cfg.target.container);
            state.subtitle_reencode_attempt = Some(codec);
            log::info!(
                "{}: retrying with subtitle re-encode (-c:s {})",
                short_name,
                codec
            );
            my_slot.progress.store(0, Ordering::Relaxed);
            my_slot.speed.store(0, Ordering::Relaxed);
            true
        }
        RetryDecision::SkipSubtitles => {
            state.skip_subs = true;
            state.subtitle_reencode_attempt = None;
            log::info!("{}: retrying without subtitles", short_name);
            ctx.flags.track_subtitle_retry(short_name);
            ctx.flags
                .track_transcode_retry(short_name, state.session_retries, "skip-subtitles");
            my_slot.progress.store(0, Ordering::Relaxed);
            my_slot.speed.store(0, Ordering::Relaxed);
            true
        }
        RetryDecision::SessionLimit => {
            state.session_retries += 1;
            ctx.ramping.store(false, Ordering::SeqCst);
            let active = ctx.active_encoders.load(Ordering::SeqCst).max(1);
            lower_max(&ctx.max_encoders, active);
            ctx.min_observed_max.fetch_min(active, Ordering::SeqCst);

            let hits = ctx.session_limit_hits.fetch_add(1, Ordering::SeqCst) + 1;
            ctx.flags.track_session_limit_hit(hits);
            ctx.flags
                .track_transcode_retry(short_name, state.session_retries, "session-limit");
            if should_freeze(hits) && !ctx.session_limit_frozen.swap(true, Ordering::SeqCst) {
                let pin = ctx
                    .min_observed_max
                    .load(Ordering::SeqCst)
                    .min(active)
                    .max(1);
                lower_max(&ctx.max_encoders, pin);
                ctx.ramping.store(false, Ordering::SeqCst);
                log::info!(
                    "NVENC session limit hit {} times; freezing max parallel encoders at {} for the rest of this run.",
                    hits, pin,
                );
            }

            *my_slot.info.lock().unwrap() = None;
            my_slot.progress.store(0, Ordering::Relaxed);
            my_slot.speed.store(0, Ordering::Relaxed);
            true
        }
        RetryDecision::Bail => false,
    }
}

/// Record a successful encode: bump byte counters, push a completed line,
/// optionally print the output path to stdout for downstream tooling.
fn record_success(
    item: &WorkItem,
    out_path: &Path,
    ctx: &WorkerCtx,
    my_slot: &Arc<WorkerSlot>,
    short_name: &str,
) {
    let out_size = transcode::output_size(out_path);
    ctx.bytes_input
        .fetch_add(item.source_size, Ordering::Relaxed);
    ctx.bytes_output.fetch_add(out_size, Ordering::Relaxed);
    if item.source_size > out_size {
        ctx.bytes_saved
            .fetch_add(item.source_size - out_size, Ordering::Relaxed);
    }
    ctx.transcoded.fetch_add(1, Ordering::Relaxed);
    let saved_pct = if item.source_size > 0 {
        ((item.source_size as f64 - out_size as f64) / item.source_size as f64 * 100.0) as i32
    } else {
        0
    };

    ctx.flags
        .track_transcode_completed(short_name, item.source_size, out_size, saved_pct);

    *my_slot.info.lock().unwrap() = None;
    my_slot.progress.store(0, Ordering::Relaxed);
    my_slot.speed.store(0, Ordering::Relaxed);

    ctx.completed_lines.lock().unwrap().push(format!(
        "  {} {} ({} {} {}, -{}%)",
        ctx.sym.check,
        short_name,
        format_size(item.source_size),
        ctx.sym.arrow,
        format_size(out_size),
        saved_pct
    ));
    if ctx.stdout_is_pipe {
        println!("{}", out_path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_freeze_at_threshold() {
        // Threshold is 3 hits (declared as a small constant; if it ever
        // changes these expected values move with it).
        assert!(!should_freeze(0));
        assert!(!should_freeze(MAX_SESSION_LIMIT_BEFORE_FREEZE - 1));
        assert!(should_freeze(MAX_SESSION_LIMIT_BEFORE_FREEZE));
        assert!(should_freeze(MAX_SESSION_LIMIT_BEFORE_FREEZE + 10));
    }

    #[test]
    fn classify_disk_space_takes_priority() {
        let s = RetryState::default();
        let d = classify_failure("Disk quota exceeded", &s);
        assert_eq!(d, RetryDecision::DiskSpace);
    }

    #[test]
    fn classify_audio_copy_fires_once_then_falls_through() {
        let mut s = RetryState::default();
        let err = "[matroska @ 0x1] No wav codec tag found for codec pcm_dvd";
        assert_eq!(classify_failure(err, &s), RetryDecision::ReencodeAudio);
        s.force_reencode_audio = true;
        // After audio re-encode attempted, identical error should NOT loop us
        // back into audio; it'd cascade through subtitle / session / Bail.
        assert_ne!(classify_failure(err, &s), RetryDecision::ReencodeAudio);
    }

    #[test]
    fn classify_subtitle_tiered() {
        let mut s = RetryState::default();
        let err = "Subtitle encoding currently only possible from text to text or bitmap to bitmap";
        assert_eq!(classify_failure(err, &s), RetryDecision::ReencodeSubtitles);
        s.subtitle_reencode_attempt = Some("srt");
        assert_eq!(classify_failure(err, &s), RetryDecision::SkipSubtitles);
        s.skip_subs = true;
        assert_eq!(classify_failure(err, &s), RetryDecision::Bail);
    }

    #[test]
    fn classify_nothing_written_routes_to_subtitle_tier() {
        // "Nothing was written" is the cascade for an incompatible subtitle codec.
        let s = RetryState::default();
        let err =
            "[out#0/matroska @ 0x1] Nothing was written into output file, because at least one of its streams received no packets.";
        assert_eq!(classify_failure(err, &s), RetryDecision::ReencodeSubtitles);
    }

    #[test]
    fn classify_session_limit_after_other_tiers_exhausted() {
        let mut s = RetryState::default();
        s.force_reencode_audio = true;
        s.skip_subs = true;
        s.subtitle_reencode_attempt = None;
        let err = "Cannot init NVENC encoder";
        assert_eq!(classify_failure(err, &s), RetryDecision::SessionLimit);
    }

    #[test]
    fn classify_session_limit_caps_at_max_retries() {
        let mut s = RetryState::default();
        s.force_reencode_audio = true;
        s.skip_subs = true;
        s.session_retries = MAX_SESSION_RETRIES;
        let err = "Cannot init NVENC encoder";
        assert_eq!(classify_failure(err, &s), RetryDecision::Bail);
    }

    #[test]
    fn classify_unknown_error_bails() {
        let s = RetryState::default();
        assert_eq!(
            classify_failure("some random error nothing matches", &s),
            RetryDecision::Bail
        );
    }

    #[test]
    fn output_stem_for_item_regular_file() {
        let p = Path::new("/media/movie.mkv");
        assert_eq!(output_stem_for_item(p, None), p);
    }

    #[test]
    fn output_stem_for_item_iso_no_suffix() {
        // Without a title suffix we return the file path unchanged — the
        // worker / partition code knows to swap .iso for the target container
        // via `transcode::output_path`.
        let p = Path::new("/media/Movie.iso");
        assert_eq!(output_stem_for_item(p, None), p);
    }

    #[test]
    fn output_stem_for_item_title_suffix_embeds_in_stem() {
        let p = Path::new("/media/Show S01D01.iso");
        let stem = output_stem_for_item(p, Some("title02"));
        assert_eq!(stem, Path::new("/media/Show S01D01.title02.iso"));
    }

    #[test]
    fn output_stem_for_item_title_suffix_distinguishes_outputs() {
        let p = Path::new("/media/Disc.iso");
        let s1 = output_stem_for_item(p, Some("title01"));
        let s2 = output_stem_for_item(p, Some("title02"));
        assert_ne!(s1, s2);
        assert_eq!(s1.parent(), p.parent());
    }

    #[test]
    fn try_acquire_succeeds_below_max() {
        let active = AtomicU32::new(0);
        let max = AtomicU32::new(3);
        assert!(try_acquire(&active, &max));
        assert_eq!(active.load(Ordering::SeqCst), 1);
        assert!(try_acquire(&active, &max));
        assert!(try_acquire(&active, &max));
        assert!(!try_acquire(&active, &max));
    }

    #[test]
    fn lower_max_never_raises() {
        let max = AtomicU32::new(3);
        lower_max(&max, 5);
        assert_eq!(max.load(Ordering::SeqCst), 3);
        lower_max(&max, 2);
        assert_eq!(max.load(Ordering::SeqCst), 2);
        lower_max(&max, 0);
        assert_eq!(max.load(Ordering::SeqCst), 0);
    }
}
