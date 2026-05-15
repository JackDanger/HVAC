//! Phase 2: probe each [`ScanItem`] and decide what becomes a [`WorkItem`].
//!
//! Five outcomes per item:
//!
//! 1. **Skip — already h265 + within target bounds** (`probe::meets_target`).
//! 2. **Skip — too short** (animated GIFs, single-frame stubs).
//! 3. **Skip — 10-bit on a non-10-bit-HEVC GPU** (Maxwell, early Pascal).
//! 4. **Skip — dest dir isn't writable** (read-only mount, ACL deny).
//! 5. **Resume** — output already exists with a valid completion marker;
//!    either swap it for the source (`--overwrite`) or count as resumed.
//! 6. **Queue** — survives every gate above; becomes a [`WorkItem`].
//!
//! Errors that prevent probing entirely (ffprobe died, file vanished) bump
//! the error counter and emit a skip line; they don't fail the run.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli::Cli;
use crate::config::Config;
use crate::gpu::GpuInfo;
use crate::iso;
use crate::probe;
use crate::transcode;

use super::worker::output_stem_for_item;
use super::{ScanItem, WorkItem};

#[cfg(test)]
use super::MIN_TRANSCODE_DURATION_SECS;

/// Result of Phase 2.
pub struct PartitionResult {
    pub to_transcode: Vec<WorkItem>,
    pub skipped: u32,
    pub resumed: u32,
    pub errors: u32,
}

/// Run Phase 2 against an already-expanded list of `ScanItem`s, deciding what
/// becomes a `WorkItem` and what's skipped / resumed.
///
/// Side-effects: emits a one-line skip note for each item we skip. Doesn't
/// touch the filesystem beyond the writable probe (which honours `--dry-run`).
/// Run Phase 2. `on_item(done, message)` is called after each file is
/// classified: `done` is the count processed so far, `message` is a
/// user-visible line (skip / resume / error) or `None` for silent skips and
/// files queued for transcode.
pub fn partition(
    items: &[ScanItem],
    cli: &Cli,
    cfg: &Config,
    gpu: &GpuInfo,
    on_item: impl Fn(usize, Option<String>),
) -> PartitionResult {
    let mut to_transcode = Vec::with_capacity(items.len());
    let mut skipped = 0u32;
    let mut resumed = 0u32;
    let mut errors = 0u32;
    let mut writable_cache: HashMap<PathBuf, bool> = HashMap::new();
    let mut done = 0usize;

    let probe_timeout = Duration::from_secs(cli.probe_timeout);
    let overwrite = cli.overwrite();

    for item in items {
        let outcome = classify_item(
            item,
            cli,
            cfg,
            gpu,
            overwrite,
            probe_timeout,
            &mut writable_cache,
        );
        done += 1;
        match outcome {
            Outcome::Transcode(w) => {
                to_transcode.push(*w);
                on_item(done, None);
            }
            Outcome::Skip(reason) => {
                on_item(done, Some(format!("  {}", reason)));
                skipped += 1;
            }
            Outcome::SkippedSilently => {
                on_item(done, None);
                skipped += 1;
            }
            Outcome::Resumed(msg) => {
                on_item(done, msg.map(|m| format!("  {}", m)));
                resumed += 1;
            }
            Outcome::ProbeError(msg) => {
                on_item(done, Some(format!("  {}", msg)));
                errors += 1;
            }
        }
    }

    PartitionResult {
        to_transcode,
        skipped,
        resumed,
        errors,
    }
}

enum Outcome {
    /// Survived every gate; queue this for transcode.
    /// Boxed because WorkItem is ~200 bytes and the other variants are small;
    /// without boxing every `Outcome` value pays the cost.
    Transcode(Box<WorkItem>),
    /// Skipped with a user-visible reason ("skip: foo.mkv: ...").
    Skip(String),
    /// Skipped (already meets target) — no per-file output, only the count.
    SkippedSilently,
    /// An existing `.transcoded.*` was adopted or just counted.
    Resumed(Option<String>),
    /// ffprobe failed entirely; counts as an error in the final summary.
    ProbeError(String),
}

fn classify_item(
    item: &ScanItem,
    cli: &Cli,
    cfg: &Config,
    gpu: &GpuInfo,
    overwrite: bool,
    probe_timeout: Duration,
    writable_cache: &mut HashMap<PathBuf, bool>,
) -> Outcome {
    let probe_result = match (&item.iso_path, &item.inner_path) {
        (Some(ip), Some(inner)) => probe::probe_iso_file_with_timeout(ip, inner, probe_timeout),
        _ => probe::probe_file_with_timeout(&item.file, probe_timeout),
    };
    let info = match probe_result {
        Ok(i) => i,
        Err(e) => {
            return Outcome::ProbeError(format!(
                "skip: {:?}: {}",
                item.file.file_name().unwrap_or_default(),
                e
            ));
        }
    };

    // Skip: too short.
    if is_too_short(info.duration_secs, cli.min_duration) {
        return Outcome::Skip(format!(
            "skip: {}: duration too short ({:.2}s)",
            item.file.display(),
            info.duration_secs
        ));
    }

    // Skip: already h265 + within target.
    if probe::meets_target(&info, &cfg.target) {
        return Outcome::SkippedSilently;
    }

    // Skip: 10-bit source against a GPU NVENC that can't encode 10-bit HEVC.
    if probe::is_10bit(&info.pix_fmt) && !gpu.supports_10bit_hevc {
        let name = item.file.file_name().unwrap_or_default().to_string_lossy();
        return Outcome::Skip(format!(
            "skip: {}: source is 10-bit; this GPU's NVENC doesn't support 10-bit HEVC.\n\
             \tUse a Turing (RTX 20xx) or newer card, or convert to 8-bit first.",
            name
        ));
    }

    // Gather size + per-file probes for multi-file ISOs.
    let source_size = source_size_for(item);
    let extra_probes = probe_extra_inner_files(item);
    let multi_file_count = item.inner_paths.as_ref().map(|p| p.len()).unwrap_or(0);
    let (bitrate_kbps, duration_secs) =
        aggregate_iso_probes(&info, &extra_probes, multi_file_count);

    // Resume / adopt: if the output already exists AND is marker-validated,
    // either swap it in for the source (in-place mode) or just count as
    // resumed. Without the marker the output is suspect — delete and re-encode.
    if let Some(outcome) = try_resume(item, cli, cfg, overwrite, duration_secs) {
        return outcome;
    }

    // Pre-flight: confirm destination dir is actually writable.
    if !cli.dry_run {
        if let Some(skip) = preflight_writable(item, cli, cfg, overwrite, writable_cache) {
            return Outcome::Skip(skip);
        }
    }

    let color = transcode::ColorMetadata::from_media_info(&info);
    // For ISO/disc items, select the primary audio stream now so the worker
    // can pass `-map 0:a:N` to ffmpeg. Regular files get None (ffmpeg maps all).
    //
    // Year hint comes from the bare filename, not the full path — a parent
    // directory like `/movies/2003-collection/` would otherwise be picked
    // up as the film's year and clobber a "(1941)" in the actual filename.
    //
    // When the selection is ambiguous AND the user opted into skipping
    // (`--skip-ambiguous-audio` on the CLI or `skip_ambiguous_audio: true`
    // in the config), bail on the disc with a clear reason. Otherwise we
    // still encode but emit a warning so an audit log can catch the
    // judgment calls after the fact.
    let primary_audio_index = if item.iso_path.is_some() {
        let filename = item
            .file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let year = probe::year_hint_for(&filename, &info.audio_streams);
        let selection = probe::pick_primary_audio(&info.audio_streams, year);
        let strict = cli.skip_ambiguous_audio || cfg.skip_ambiguous_audio;
        match selection {
            Some(sel) if sel.ambiguous && strict => {
                return Outcome::Skip(format!(
                    "skip: {}: ambiguous primary audio ({})",
                    item.file.display(),
                    sel.reason.as_deref().unwrap_or("no clear primary track"),
                ));
            }
            Some(sel) => {
                if sel.ambiguous {
                    log::warn!(
                        "{}: ambiguous primary audio selection (chose a:{}, {})",
                        item.file.display(),
                        sel.index,
                        sel.reason.as_deref().unwrap_or("?"),
                    );
                }
                Some(sel.index)
            }
            None => None,
        }
    } else {
        None
    };
    Outcome::Transcode(Box::new(WorkItem {
        path: item.file.clone(),
        bitrate_kbps,
        duration_secs,
        pix_fmt: info.pix_fmt,
        source_size,
        color,
        iso_path: item.iso_path.clone(),
        inner_path: item.inner_path.clone(),
        inner_paths: item.inner_paths.clone(),
        title_suffix: item.title_suffix.clone(),
        primary_audio_index,
    }))
}

/// Returns true when the file is short enough to skip.
/// `0.0` (or negative) means ffprobe couldn't determine duration; we let
/// those fall through to the normal codepath rather than silently dropping.
pub fn is_too_short(duration_secs: f64, min_duration_secs: f64) -> bool {
    duration_secs > 0.0 && duration_secs < min_duration_secs
}

/// Sum the inner-file sizes (ISO multi-file) or use `fs::metadata` (regular file).
fn source_size_for(item: &ScanItem) -> u64 {
    if let Some(ref ip) = item.iso_path {
        if let Some(ref paths) = item.inner_paths {
            return paths
                .iter()
                .filter_map(|p| iso::file_size(ip, p).ok())
                .sum();
        }
        if let Some(ref inner) = item.inner_path {
            return iso::file_size(ip, inner).unwrap_or(0);
        }
        return 0;
    }
    std::fs::metadata(&item.file).map(|m| m.len()).unwrap_or(0)
}

/// Probe inner files 2..N for a multi-file ISO main feature. First file was
/// already probed by the caller; skipping it here avoids the double cost.
fn probe_extra_inner_files(item: &ScanItem) -> Vec<Option<probe::MediaInfo>> {
    let (Some(ip), Some(paths)) = (&item.iso_path, &item.inner_paths) else {
        return Vec::new();
    };
    if paths.len() <= 1 {
        return Vec::new();
    }
    paths
        .iter()
        .skip(1)
        .map(|inner| match probe::probe_iso_file(ip, inner) {
            Ok(info) => Some(info),
            Err(e) => {
                log::debug!(
                    "  per-file probe failed for {}:{}: {}",
                    ip.display(),
                    inner,
                    e
                );
                None
            }
        })
        .collect()
}

/// Combine the representative probe (first inner file of an ISO main feature)
/// with per-file probes of the remaining inner files into
/// `(bitrate_kbps, duration_secs)`.
///
/// * `bitrate_kbps` is the **maximum** across all successful probes. Using
///   max prevents `-maxrate` from throttling later, higher-bitrate VOBs.
/// * `duration_secs` is the **sum** of every probe's duration. If any per-file
///   probe failed we fall back to `representative.duration_secs * total_count`
///   since the sum would otherwise undercount.
/// * `total_file_count` is the total number of inner files (including the
///   representative). `0` or `1` means "not multi-file" — return the
///   representative's values unchanged.
pub fn aggregate_iso_probes(
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

/// Adopt-resume gate. Returns `Some(Outcome)` if the item should be skipped
/// or resumed; `None` to fall through to a fresh encode.
///
/// The marker check is critical: ffprobe-only validation can pass on a
/// half-written `.transcoded.*` left from a killed previous run. We require
/// a `<output>.hvac.complete` sidecar (written only after the previous
/// run's final-rename succeeded) before adopting.
fn try_resume(
    item: &ScanItem,
    cli: &Cli,
    cfg: &Config,
    overwrite: bool,
    duration_secs: f64,
) -> Option<Outcome> {
    let out_dir = cli.output_dir.as_deref().or(cfg.output_dir.as_deref());
    let source_for_output = output_stem_for_item(&item.file, item.title_suffix.as_deref());
    let out_path =
        transcode::output_path(&source_for_output, out_dir, &cfg.target.container).ok()?;

    if !transcode::output_already_valid(&out_path, &item.file, duration_secs) {
        return None;
    }

    // ffprobe-only validation has passed, but that alone is unsafe: a half-
    // written file with a valid header and plausible duration can pass.
    // Require a sidecar `.hvac.complete` marker that matches the current
    // source size — written only by a fully successful previous run.
    if !transcode::marker_valid_for_source(&out_path, &item.file) {
        eprintln!(
            "  Ignoring incomplete previous output for {:?} (no/stale {} marker) — re-encoding",
            item.file.file_name().unwrap_or_default(),
            transcode::MARKER_SUFFIX
        );
        let _ = std::fs::remove_file(&out_path);
        transcode::remove_marker(&out_path);
        return None;
    }

    // Never rename a `.transcoded` file back over a disc image — the ISO/IMG
    // is the source, not the destination.
    let is_disc = item.iso_path.is_some();
    if overwrite && !is_disc {
        match transcode::replace_original(&item.file, &out_path, duration_secs) {
            Ok(_saved) => Some(Outcome::Resumed(Some(format!(
                "Replaced {:?} with existing transcoded copy",
                item.file.file_name().unwrap_or_default()
            )))),
            Err(e) => {
                eprintln!(
                    "  Failed to replace {:?}: {}",
                    item.file.file_name().unwrap_or_default(),
                    e
                );
                None
            }
        }
    } else {
        Some(Outcome::Resumed(None))
    }
}

/// Verify the destination directory is writable, mirroring the worker's
/// actual output-path branching (in-place vs. output_dir vs. source-parent).
/// Returns `Some(skip_message)` if not writable, `None` to proceed.
fn preflight_writable(
    item: &ScanItem,
    cli: &Cli,
    cfg: &Config,
    overwrite: bool,
    cache: &mut HashMap<PathBuf, bool>,
) -> Option<String> {
    let out_dir = cli.output_dir.as_deref().or(cfg.output_dir.as_deref());
    let dest_dir: PathBuf = if overwrite && item.iso_path.is_none() {
        // In-place mode: worker writes next to the source.
        item.file
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    } else if let Some(d) = out_dir {
        // Worker will create_dir_all this path before writing; do the same
        // here so the probe doesn't ENOENT on a dir that's about to exist.
        if !d.exists() {
            if let Err(e) = std::fs::create_dir_all(d) {
                let name = item.file.file_name().unwrap_or_default().to_string_lossy();
                return Some(format!(
                    "skip: {}: cannot create output directory {:?}: {}",
                    name, d, e
                ));
            }
        }
        d.to_path_buf()
    } else {
        item.file
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    };

    if dir_is_writable_cached(cache, &dest_dir) {
        return None;
    }

    let name = item.file.file_name().unwrap_or_default().to_string_lossy();
    if overwrite && item.iso_path.is_none() {
        Some(format!(
            "skip: {}: source directory is not writable; \
             use --no-overwrite to write transcodes elsewhere",
            name
        ))
    } else {
        Some(format!(
            "skip: {}: output directory {:?} is not writable",
            name, dest_dir
        ))
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

/// Probe whether `dir` allows file creation by attempting to create (and
/// immediately remove) a uniquely-named file inside it.
///
/// Uses `OpenOptions::create_new` + a nanosecond timestamp so the probe
/// can't clobber a user file via PID collision; treats remove-failure as
/// a probe failure so we don't leave stray probe files in user media dirs.
pub fn dir_is_writable(dir: &Path) -> bool {
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

    if OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .is_err()
    {
        return false;
    }

    match std::fs::remove_file(&probe) {
        Ok(()) => true,
        Err(e) => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn mk_info(bitrate_kbps: u32, duration_secs: f64) -> probe::MediaInfo {
        probe::MediaInfo {
            codec: "mpeg2video".to_string(),
            width: 720,
            height: 480,
            bitrate_kbps,
            duration_secs,
            pix_fmt: "yuv420p".to_string(),
            has_audio: true,
            has_subtitles: false,
            ..probe::MediaInfo::default()
        }
    }

    #[test]
    fn is_too_short_below_threshold() {
        assert!(is_too_short(0.04, 1.0));
        assert!(is_too_short(0.999, 1.0));
    }

    #[test]
    fn is_too_short_at_threshold_returns_false() {
        // Strict less-than: equal is not too short.
        assert!(!is_too_short(1.0, 1.0));
        assert!(!is_too_short(1.5, 1.0));
    }

    #[test]
    fn is_too_short_zero_falls_through() {
        // ffprobe-couldn't-determine sentinel.
        assert!(!is_too_short(0.0, 1.0));
        assert!(!is_too_short(-1.0, 1.0));
    }

    #[test]
    fn is_too_short_with_min_zero_disables_skip() {
        assert!(!is_too_short(0.04, 0.0));
        assert!(!is_too_short(1.0, 0.0));
    }

    #[test]
    fn min_transcode_duration_default_is_one_second() {
        assert_eq!(MIN_TRANSCODE_DURATION_SECS, 1.0);
    }

    #[test]
    fn aggregate_iso_probes_single_file_returns_representative() {
        let rep = mk_info(4000, 1500.0);
        let (b, d) = aggregate_iso_probes(&rep, &[], 1);
        assert_eq!(b, 4000);
        assert!((d - 1500.0).abs() < 1e-9);
    }

    #[test]
    fn aggregate_iso_probes_takes_max_bitrate() {
        let rep = mk_info(2000, 500.0);
        let extras = vec![
            Some(mk_info(8000, 500.0)),
            Some(mk_info(4000, 500.0)),
            Some(mk_info(6000, 500.0)),
        ];
        let (b, _d) = aggregate_iso_probes(&rep, &extras, 4);
        assert_eq!(b, 8000, "should pick the highest bitrate");
    }

    #[test]
    fn aggregate_iso_probes_sums_durations_when_all_probes_succeed() {
        let rep = mk_info(2000, 500.0);
        let extras = vec![Some(mk_info(2000, 600.0)), Some(mk_info(2000, 700.0))];
        let (_b, d) = aggregate_iso_probes(&rep, &extras, 3);
        assert!((d - 1800.0).abs() < 1e-9);
    }

    #[test]
    fn aggregate_iso_probes_falls_back_when_any_probe_failed() {
        let rep = mk_info(2000, 500.0);
        let extras = vec![Some(mk_info(2000, 600.0)), None];
        let (_b, d) = aggregate_iso_probes(&rep, &extras, 3);
        // Falls back to rep.duration * count = 500 * 3 = 1500.
        assert!((d - 1500.0).abs() < 1e-9);
    }

    #[test]
    fn dir_is_writable_for_temp_dir() {
        let tmp = tempdir().unwrap();
        assert!(dir_is_writable(tmp.path()));
        // No probe file should remain.
        let leftover: Vec<_> = fs::read_dir(tmp.path())
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
    fn dir_is_writable_cache_returns_same_result_twice() {
        let tmp = tempdir().unwrap();
        let mut cache = HashMap::new();
        assert!(dir_is_writable_cached(&mut cache, tmp.path()));
        assert_eq!(cache.len(), 1);
        // Second call hits cache (still len 1, still true).
        assert!(dir_is_writable_cached(&mut cache, tmp.path()));
        assert_eq!(cache.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn dir_is_writable_false_for_readonly() {
        use std::os::unix::fs::PermissionsExt;
        let is_root = unsafe { libc::geteuid() } == 0;
        if is_root {
            eprintln!("skipping read-only check under root (DAC bypass)");
            return;
        }
        let tmp = tempdir().unwrap();
        let ro = tmp.path().join("ro");
        fs::create_dir(&ro).unwrap();
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
        let writable = dir_is_writable(&ro);
        let _ = fs::set_permissions(&ro, fs::Permissions::from_mode(0o755));
        assert!(!writable);
    }

    #[test]
    fn dir_is_writable_does_not_clobber_user_file() {
        // Plant a file with the deterministic-PID legacy name. The new
        // probe must not touch it (different name, and create_new wouldn't
        // truncate it even if it did collide).
        let tmp = tempdir().unwrap();
        let user_file = tmp
            .path()
            .join(format!(".hvac_writable_check_{}", std::process::id()));
        fs::write(&user_file, b"user content").unwrap();
        for _ in 0..3 {
            assert!(dir_is_writable(tmp.path()));
        }
        assert_eq!(fs::read(&user_file).unwrap(), b"user content");
    }
}
