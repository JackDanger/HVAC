//! Phase 1: expand on-disk paths into [`ScanItem`]s.
//!
//! Regular files map 1:1. Disc images (`.iso`, `.img`) are inspected with the
//! `isomage` crate's filesystem parser and emit one or more ScanItems:
//!
//!   - **Single-feature disc** → one ScanItem.
//!   - **Multi-file main feature** (DVD VOBs, Blu-ray chapter m2ts) → one
//!     ScanItem with `inner_paths` populated for concatenation.
//!   - **Multi-title disc** (TV-episode DVD with N similar-sized title sets)
//!     → one ScanItem per title, each with `title_suffix = "titleNN"`.
//!   - **AACS / BD+ encrypted** → skipped with a clear message; ffmpeg can't
//!     decode them and feeding garbage to the encoder mid-run wastes time.
//!   - **No media files inside** → skipped.
//!
//! Disc-analysis failures count toward the `errors` return value; skips do not.

use std::path::{Path, PathBuf};

use crate::iso;

use super::ScanItem;

/// Return value of [`expand`]: the flat work list plus the count of disc
/// images we couldn't analyse at all (corrupt ISOs etc.). Skipped-but-known
/// cases (encrypted, no-media-inside) are NOT errors — they're surfaced via
/// stderr at the time of the skip but don't bump this counter.
pub struct ScanResult {
    pub items: Vec<ScanItem>,
    pub errors: u32,
}

/// Expand each path in `files` into one or more [`ScanItem`]s.
///
/// Side-effect: writes a one-line note to stderr for each disc image
/// encountered, plus a skip line for each disc we don't process (encrypted,
/// empty, or analysis-failed). The caller doesn't need to repeat these.
pub fn expand(files: &[PathBuf]) -> ScanResult {
    let mut items = Vec::with_capacity(files.len());
    let mut errors = 0u32;

    for file in files {
        if iso::is_disc_image(file) {
            let iso_name = file.file_name().unwrap_or_default().to_string_lossy();

            let analysis = match iso::analyze_disc(file) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("  skip: {}: {}", iso_name, e);
                    errors += 1;
                    continue;
                }
            };

            // Encrypted commercial discs (AACS / BD+) can't be decoded by ffmpeg.
            // Skip with a clear, actionable message rather than aborting the run.
            if let iso::DiscType::Encrypted { ref reason } = analysis.disc_type {
                eprintln!("  skip: {}: {}", iso_name, reason);
                continue;
            }

            if analysis.main_feature.is_empty() {
                eprintln!("  skip: {}: no media files inside", iso_name);
                continue;
            }

            // Multi-title disc (e.g. TV-episode DVD): emit one ScanItem per title.
            if let Some(groups) = analysis.multi_title_groups.as_ref() {
                eprintln!(
                    "  iso: {} ({:?}, {} titles, {} extras)",
                    iso_name,
                    analysis.disc_type,
                    groups.len(),
                    analysis.extras.len(),
                );
                for (i, group) in groups.iter().enumerate() {
                    if group.is_empty() {
                        continue;
                    }
                    let suffix = format!("title{:02}", i + 1);
                    items.push(iso_item_from_group(file, group, Some(suffix)));
                }
                continue;
            }

            eprintln!(
                "  iso: {} ({:?}, {} main feature files, {} extras)",
                iso_name,
                analysis.disc_type,
                analysis.main_feature.len(),
                analysis.extras.len(),
            );

            items.push(iso_item_from_group(file, &analysis.main_feature, None));
            continue;
        }

        items.push(ScanItem {
            file: file.clone(),
            iso_path: None,
            inner_path: None,
            inner_paths: None,
            title_suffix: None,
        });
    }

    ScanResult { items, errors }
}

/// Build one ScanItem from an ISO `iso_file` and one of its inner main-feature
/// groups (DVD title set, Blu-ray chapter run, or full main feature).
fn iso_item_from_group(
    iso_file: &Path,
    group: &[iso::IsoMediaFile],
    suffix: Option<String>,
) -> ScanItem {
    let paths: Vec<String> = group.iter().map(|f| f.path.clone()).collect();
    let (inner_paths, inner_path) = if paths.len() == 1 {
        (None, Some(paths[0].clone()))
    } else {
        (Some(paths.clone()), Some(paths[0].clone()))
    };
    ScanItem {
        file: iso_file.to_path_buf(),
        iso_path: Some(iso_file.to_path_buf()),
        inner_path,
        inner_paths,
        title_suffix: suffix,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn expand_regular_files_pass_through_unchanged() {
        let dir = tempdir().unwrap();
        let mkv = dir.path().join("video.mkv");
        let mp4 = dir.path().join("clip.mp4");
        fs::write(&mkv, b"x").unwrap();
        fs::write(&mp4, b"x").unwrap();

        let r = expand(&[mkv.clone(), mp4.clone()]);
        assert_eq!(r.items.len(), 2);
        assert_eq!(r.errors, 0);

        assert_eq!(r.items[0].file, mkv);
        assert!(r.items[0].iso_path.is_none());
        assert!(r.items[0].inner_paths.is_none());
        assert!(r.items[0].title_suffix.is_none());
    }

    #[test]
    fn expand_skips_non_iso_when_iso_path_present() {
        // Sanity: a regular .mkv produces an item with iso_path = None.
        let dir = tempdir().unwrap();
        let mkv = dir.path().join("normal.mkv");
        fs::write(&mkv, b"x").unwrap();
        let r = expand(&[mkv]);
        assert!(r.items[0].iso_path.is_none());
        assert!(r.items[0].inner_path.is_none());
    }

    #[test]
    fn iso_item_from_group_single_file_uses_inner_path_not_paths() {
        let iso = Path::new("/media/Movie.iso");
        let group = vec![iso::IsoMediaFile {
            path: "BDMV/STREAM/00000.M2TS".to_string(),
            size: 1000,
        }];
        let item = iso_item_from_group(iso, &group, None);
        assert_eq!(item.file, Path::new("/media/Movie.iso"));
        assert_eq!(item.iso_path.as_deref(), Some(iso));
        assert_eq!(item.inner_path.as_deref(), Some("BDMV/STREAM/00000.M2TS"));
        assert!(
            item.inner_paths.is_none(),
            "single-file ISO must not populate inner_paths"
        );
        assert!(item.title_suffix.is_none());
    }

    #[test]
    fn iso_item_from_group_multi_file_populates_inner_paths() {
        let iso = Path::new("/media/Movie.iso");
        let group = vec![
            iso::IsoMediaFile {
                path: "VIDEO_TS/VTS_01_1.VOB".to_string(),
                size: 1000,
            },
            iso::IsoMediaFile {
                path: "VIDEO_TS/VTS_01_2.VOB".to_string(),
                size: 1000,
            },
        ];
        let item = iso_item_from_group(iso, &group, None);
        assert_eq!(
            item.inner_path.as_deref(),
            Some("VIDEO_TS/VTS_01_1.VOB"),
            "inner_path must be the first file (the representative for probing)"
        );
        let paths = item.inner_paths.as_ref().expect("multi-file → Some");
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], "VIDEO_TS/VTS_01_1.VOB");
        assert_eq!(paths[1], "VIDEO_TS/VTS_01_2.VOB");
    }

    #[test]
    fn iso_item_from_group_carries_title_suffix() {
        let iso = Path::new("/media/Disc.iso");
        let group = vec![iso::IsoMediaFile {
            path: "VIDEO_TS/VTS_03_1.VOB".to_string(),
            size: 1,
        }];
        let item = iso_item_from_group(iso, &group, Some("title03".to_string()));
        assert_eq!(item.title_suffix.as_deref(), Some("title03"));
    }
}
