use anyhow::Result;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::iso;

/// Recursively collect all files under `dir`, following symlinks.
/// Tracks visited canonical directory paths to prevent infinite symlink cycles.
/// Calls `on_progress` every 1000 files with current directory and counts.
fn walk_files(
    dir: &Path,
    out: &mut Vec<PathBuf>,
    visited_dirs: &mut HashSet<PathBuf>,
    on_progress: &mut dyn FnMut(&Path, usize),
    files_since_update: &mut usize,
) {
    let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    if !visited_dirs.insert(canonical) {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        // Use metadata() (follows symlinks) so symlinked dirs/files are handled correctly
        match std::fs::metadata(&path) {
            Ok(m) if m.is_dir() => {
                walk_files(&path, out, visited_dirs, on_progress, files_since_update)
            }
            Ok(m) if m.is_file() => {
                out.push(path);
                *files_since_update += 1;
                if *files_since_update >= 1000 {
                    on_progress(dir, out.len());
                    *files_since_update = 0;
                }
            }
            _ => {}
        }
    }
}

/// Scan a directory tree for media files matching the given extensions.
/// Also picks up .iso and .img disc images.
/// Deduplicates by canonical path to avoid encoding the same file twice
/// when symlinks create multiple paths to the same file.
/// If `root` is a file rather than a directory, it is treated as the sole input.
pub fn scan(root: &Path, extensions: &[String]) -> Result<Vec<PathBuf>> {
    let ext_lower: Vec<String> = extensions.iter().map(|e| e.to_lowercase()).collect();

    // Single-file mode: path points directly at a media file.
    if root.is_file() {
        let passes = iso::is_disc_image(root)
            || root
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| ext_lower.contains(&e.to_lowercase()))
                .unwrap_or(false);
        if passes {
            return Ok(vec![root.to_path_buf()]);
        }
        return Ok(vec![]);
    }

    let in_screen = std::env::var("STY").is_ok();

    let mut all_files = Vec::new();
    let mut files_since_update = 0;
    if in_screen {
        // In screen, spinner causes newline spam. Use simple logging instead.
        let mut on_progress = |dir: &Path, total: usize| {
            let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("...");
            eprintln!("  Scanning: {} [{} files found]", dir_name, total);
        };
        walk_files(
            root,
            &mut all_files,
            &mut HashSet::new(),
            &mut on_progress,
            &mut files_since_update,
        );
    } else {
        let spinner = indicatif::ProgressBar::new_spinner();
        spinner.set_style(
            indicatif::ProgressStyle::default_spinner()
                .template("{spinner:.cyan} {msg}")
                .unwrap(),
        );

        let mut on_progress = |dir: &Path, total: usize| {
            let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("...");
            spinner.set_message(format!("Scanning: {} [{} files found]", dir_name, total));
            spinner.tick();
        };

        walk_files(
            root,
            &mut all_files,
            &mut HashSet::new(),
            &mut on_progress,
            &mut files_since_update,
        );
        spinner.finish_and_clear();
    }

    let mut seen = HashSet::new();
    let mut files: Vec<PathBuf> = all_files
        .into_iter()
        .filter(|path| {
            // Skip tdorr temporary and transcoded output files
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with(".tdorr_tmp_") {
                    return false;
                }
                if name.contains(".transcoded.") {
                    return false;
                }
            }
            if iso::is_disc_image(path) {
                return true;
            }
            path.extension()
                .and_then(|e| e.to_str())
                .map(|e| ext_lower.contains(&e.to_lowercase()))
                .unwrap_or(false)
        })
        .filter(|path| {
            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
            seen.insert(canonical)
        })
        .collect();

    files.sort();

    log::debug!("Scanned {:?}: found {} media files", root, files.len());

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_scan_finds_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("video.mkv"), "fake").unwrap();
        fs::write(dir.path().join("video.mp4"), "fake").unwrap();
        fs::write(dir.path().join("readme.txt"), "fake").unwrap();

        let exts = vec!["mkv".to_string(), "mp4".to_string()];
        let files = scan(dir.path(), &exts).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_scan_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let exts = vec!["mkv".to_string()];
        let files = scan(dir.path(), &exts).unwrap();
        assert_eq!(files.len(), 0);
    }

    #[test]
    fn test_scan_nested_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("Season 1");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("ep01.mkv"), "fake").unwrap();
        fs::write(sub.join("ep02.mkv"), "fake").unwrap();

        let exts = vec!["mkv".to_string()];
        let files = scan(dir.path(), &exts).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_scan_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("video.MKV"), "fake").unwrap();

        let exts = vec!["mkv".to_string()];
        let files = scan(dir.path(), &exts).unwrap();
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn test_scan_skips_transcoded_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("episode.mkv"), "fake").unwrap();
        fs::write(dir.path().join("episode.transcoded.mkv"), "fake").unwrap();

        let exts = vec!["mkv".to_string()];
        let files = scan(dir.path(), &exts).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].file_name().unwrap().to_str().unwrap() == "episode.mkv");
    }

    #[test]
    fn test_scan_skips_tmp_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("episode.mkv"), "fake").unwrap();
        fs::write(dir.path().join(".tdorr_tmp_episode.mkv"), "fake").unwrap();

        let exts = vec!["mkv".to_string()];
        let files = scan(dir.path(), &exts).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].file_name().unwrap().to_str().unwrap() == "episode.mkv");
    }

    #[test]
    fn test_scan_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("movie.mkv");
        fs::write(&file, "fake").unwrap();

        let exts = vec!["mkv".to_string()];
        let files = scan(&file, &exts).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], file);
    }

    #[test]
    fn test_scan_single_file_wrong_ext() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("document.txt");
        fs::write(&file, "fake").unwrap();

        let exts = vec!["mkv".to_string()];
        let files = scan(&file, &exts).unwrap();
        assert_eq!(files.len(), 0);
    }

    #[test]
    fn test_scan_dedup_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let season1 = dir.path().join("Season 1");
        fs::create_dir(&season1).unwrap();
        fs::write(season1.join("ep01.mkv"), "fake").unwrap();

        // Create a symlink that points to the same directory
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&season1, dir.path().join("Season 01")).unwrap();
            let exts = vec!["mkv".to_string()];
            let files = scan(dir.path(), &exts).unwrap();
            assert_eq!(files.len(), 1, "symlinked duplicate should be deduplicated");
        }
    }
}
