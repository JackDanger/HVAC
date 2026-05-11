//! Phase 4: optional `--replace` pass that swaps originals with `.transcoded.*`.
//!
//! Only fires when `cli.replace && !overwrite` (replace originals after a
//! `--no-overwrite` run). Each item's `.transcoded.<ext>` sibling is
//! re-validated by ffprobe before the rename so a half-finished output
//! doesn't take out the source.

use crate::cli::Cli;
use crate::config::Config;
use crate::transcode;
use crate::ui::Symbols;
use crate::util::format_size;

use super::WorkItem;

/// Result of Phase 4: how many originals were replaced and the cumulative
/// bytes saved (size difference between original and replacement). Returned
/// for callers that want to surface these in their own summary; the current
/// orchestrator only consumes them via the eprintln! below.
#[allow(dead_code)]
pub struct ReplaceResult {
    pub replaced: u32,
    pub saved_bytes: u64,
}

/// Run Phase 4 against the list of already-transcoded WorkItems. Skips
/// per-item errors with a one-line note and continues.
pub fn run(items: &[WorkItem], cli: &Cli, cfg: &Config, sym: &Symbols) -> ReplaceResult {
    let mut replaced = 0u32;
    let mut saved_bytes = 0u64;

    eprintln!("Replacing originals with transcoded copies...");
    let out_dir = cli.output_dir.as_deref().or(cfg.output_dir.as_deref());
    for item in items {
        let Ok(out_path) = transcode::output_path(&item.path, out_dir, &cfg.target.container)
        else {
            continue;
        };
        if !out_path.exists() {
            continue;
        }
        match transcode::replace_original(&item.path, &out_path, item.duration_secs) {
            Ok(saved) => {
                replaced += 1;
                saved_bytes += saved;
            }
            Err(e) => {
                eprintln!(
                    "  {} replace {:?}: {}",
                    sym.cross,
                    item.path.file_name().unwrap_or_default(),
                    e
                );
            }
        }
    }

    if replaced > 0 {
        eprintln!(
            "Replaced {} originals (saved {})",
            replaced,
            format_size(saved_bytes)
        );
    }

    ReplaceResult {
        replaced,
        saved_bytes,
    }
}
