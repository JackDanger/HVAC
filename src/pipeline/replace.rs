//! Phase 4: optional `--replace` pass that swaps originals with `.transcoded.*`.
//!
//! Only fires when `cli.replace && !overwrite` (replace originals after a
//! `--no-overwrite` run). Each item's `.transcoded.<ext>` sibling is
//! re-validated by ffprobe before the rename so a half-finished output
//! doesn't take out the source.
//!
//! Disc-image items (`iso_path.is_some()`) are skipped: their transcoded
//! output is a regular `.mkv`/`.mp4` next to the disc image, so renaming it
//! onto the `.iso` path would corrupt the source — you'd be left with an
//! MKV-payload file wearing an `.iso` extension and no disc image at all.

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
    pub skipped_disc_images: u32,
}

/// Run Phase 4 against the list of already-transcoded WorkItems. Skips
/// per-item errors with a one-line note and continues.
pub fn run(items: &[WorkItem], cli: &Cli, cfg: &Config, sym: &Symbols) -> ReplaceResult {
    let mut replaced = 0u32;
    let mut saved_bytes = 0u64;
    let mut skipped_disc_images = 0u32;

    eprintln!("Replacing originals with transcoded copies...");
    let out_dir = cli.output_dir.as_deref().or(cfg.output_dir.as_deref());
    for item in items {
        // Disc images never get rename-replaced: the transcoded output is a
        // separate-extension file and renaming it onto the disc would destroy
        // the source while leaving an MKV with an .iso suffix.
        if item.iso_path.is_some() {
            skipped_disc_images += 1;
            eprintln!(
                "  - skip {:?}: disc image, transcoded output kept alongside",
                item.path.file_name().unwrap_or_default()
            );
            continue;
        }
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
        skipped_disc_images,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcode::ColorMetadata;
    use clap::Parser as _;
    use std::path::PathBuf;

    fn work_item(path: &str, iso_path: Option<&str>) -> WorkItem {
        WorkItem {
            path: PathBuf::from(path),
            bitrate_kbps: 4000,
            duration_secs: 60.0,
            pix_fmt: "yuv420p".to_string(),
            source_size: 1024,
            color: ColorMetadata::default(),
            iso_path: iso_path.map(PathBuf::from),
            inner_path: None,
            inner_paths: None,
            title_suffix: None,
            primary_audio_index: None,
        }
    }

    #[test]
    fn replace_skips_disc_image_items() {
        // We can't run `replace_original` against the filesystem in a unit
        // test, but we can verify the skip path: every input is a disc-image
        // item, so the iterator must short-circuit on all of them and report
        // them as skipped without ever calling replace_original.
        let items = vec![
            work_item("/media/disc1.iso", Some("/media/disc1.iso")),
            work_item("/media/disc2.img", Some("/media/disc2.img")),
        ];
        let cli = Cli::try_parse_from(["hvac", "--replace", "/media"]).unwrap();
        let cfg = Config::from_embedded();

        let result = run(&items, &cli, &cfg, &crate::ui::ASCII_SYMBOLS);

        assert_eq!(result.replaced, 0);
        assert_eq!(result.saved_bytes, 0);
        assert_eq!(result.skipped_disc_images, 2);
    }
}
