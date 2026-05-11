//! Transcode pipeline: scan → partition → encode.
//!
//! The pipeline is split into four phases, each in its own submodule:
//!
//! 1. [`scan`] — expand each on-disk media path into `ScanItem`s. Disc images
//!    blow up into one or many items (single-feature, multi-file ISO main
//!    feature, multi-title episode DVDs); regular files map 1:1.
//! 2. [`partition`] — probe each `ScanItem` with ffprobe and decide whether
//!    it's already h265 (skip), too short (skip), missing target dir (skip),
//!    or queued for transcode. Survivors become `WorkItem`s.
//! 3. [`worker`] — drain the `WorkItem` queue across a pool of encode threads.
//!    Each worker handles its own retry tier (disk space → audio re-encode →
//!    subtitle re-encode → drop subs → NVENC session limit) before giving up
//!    on a file.
//! 4. [`render`] — a single thread paints the per-worker progress viewport
//!    and the overall progress bar; the worker threads only write into
//!    atomics and a `completed_lines` queue.
//!
//! [`replace`] is an optional Phase 4 driven by `--replace` that swaps
//! originals with `.transcoded.*` siblings after the encode phase completes.

pub mod partition;
pub mod render;
pub mod replace;
pub mod scan;
pub mod worker;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Mutex;

use crate::transcode;

/// Skip files shorter than this duration (seconds). Animated GIFs, single-frame
/// WebP-as-mp4 stubs, and similar artefacts pass the extension scan but produce
/// unusable output (or fail validation) when transcoded — skip them with a
/// clear message instead. Override per-run with `--min-duration <SECS>`.
pub const MIN_TRANSCODE_DURATION_SECS: f64 = 1.0;

/// A single unit of work after Phase 1's disc-image expansion.
///
/// Either represents a regular file (just `file`) or one slice of an ISO
/// main feature: ISO path + one-or-more inner paths + an optional title
/// suffix that distinguishes per-title outputs on multi-title discs.
#[derive(Debug, Clone)]
pub struct ScanItem {
    /// Original on-disk path (regular file, or the .iso/.img for ISO entries).
    pub file: PathBuf,
    /// Same as `file` for ISO entries, `None` for regular files.
    pub iso_path: Option<PathBuf>,
    /// Representative inner path used for probing; first element of inner_paths.
    pub inner_path: Option<String>,
    /// Multi-file ISO main feature, in concatenation order. None when single-file.
    pub inner_paths: Option<Vec<String>>,
    /// `"titleNN"` for one-of-many multi-title outputs; `None` for single-feature.
    pub title_suffix: Option<String>,
}

/// File ready to transcode, with pre-probed metadata.
///
/// Built by Phase 2 from `ScanItem` + the ffprobe results. The `iso_*` /
/// `inner_*` fields are forwarded straight from the originating ScanItem;
/// `title_suffix` is consumed by [`crate::pipeline::worker::output_stem_for_item`]
/// to pick the output filename.
pub struct WorkItem {
    pub path: PathBuf,
    pub bitrate_kbps: u32,
    pub duration_secs: f64,
    pub pix_fmt: String,
    pub source_size: u64,
    /// Color / HDR metadata to forward to the encoder so HDR10 / HLG / wide-
    /// gamut sources don't end up with the wrong tags on output.
    pub color: transcode::ColorMetadata,
    pub iso_path: Option<PathBuf>,
    pub inner_path: Option<String>,
    pub inner_paths: Option<Vec<String>>,
    pub title_suffix: Option<String>,
}

/// Per-worker display slot. Worker threads update these atomics + `info`
/// mutex; the render thread reads them every 200ms to repaint the viewport.
pub struct WorkerSlot {
    /// `Some((short_name, size_str))` while encoding, `None` otherwise.
    /// Behind a Mutex because we update name+size atomically together.
    pub info: Mutex<Option<(String, String)>>,
    /// 0–1000 (1000 = complete). Stored as u64 so the render thread can
    /// also sum these into the overall progress bar without an extra mutex.
    pub progress: AtomicU64,
    /// Encode speed scaled ×100 (e.g. 123 = 1.23×). Render formats as decimal.
    pub speed: AtomicU64,
    /// True when waiting for an encoding slot to free up (fixed-jobs mode).
    pub queued: AtomicBool,
    /// True when waiting for disk space to free up.
    pub disk_wait: AtomicBool,
}

impl WorkerSlot {
    /// Construct an idle slot.
    pub fn new() -> Self {
        WorkerSlot {
            info: Mutex::new(None),
            progress: AtomicU64::new(0),
            speed: AtomicU64::new(0),
            queued: AtomicBool::new(false),
            disk_wait: AtomicBool::new(false),
        }
    }

    /// Clear the slot so the render thread shows nothing for this worker.
    pub fn clear(&self) {
        use std::sync::atomic::Ordering;
        *self.info.lock().unwrap() = None;
        self.progress.store(0, Ordering::Relaxed);
        self.speed.store(0, Ordering::Relaxed);
        self.queued.store(false, Ordering::Relaxed);
        self.disk_wait.store(false, Ordering::Relaxed);
    }
}

impl Default for WorkerSlot {
    fn default() -> Self {
        Self::new()
    }
}

/// Global cancellation flag. SIGINT handler sets it; workers and the render
/// thread check it every loop iteration.
pub static CANCELLED: AtomicBool = AtomicBool::new(false);

/// Set by a worker when the LaunchDarkly `enable-transcoding` flag returns
/// false. Unlike `CANCELLED` (which triggers `process::exit(130)`), this flag
/// lets `main` fall through to the normal tracking/flush path for a clean exit.
pub static LD_KILL: AtomicBool = AtomicBool::new(false);

/// Directories where `.hvac_tmp_*` files may exist during a run.
/// Populated as encodes start; read by the SIGINT-twice force-quit handler.
pub static TMP_DIRS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Remove in-progress `.hvac_tmp_*` encode scratch files from every directory
/// registered in [`TMP_DIRS`].
///
/// Deliberately does **not** touch `.hvac.complete` sidecar markers: those are
/// written *after* the final rename succeeds (so a SIGINT can never strand one
/// next to a partial output), and any markers from prior runs are part of the
/// resume/adopt contract — clearing them here would force unnecessary re-encodes
/// on the next run.
pub fn cleanup_tmp_dirs() {
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
                if name.starts_with(".hvac_tmp_") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}
