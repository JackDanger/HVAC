use anyhow::Result;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::iso;

/// Scan a directory tree for media files matching the given extensions.
/// Also picks up .iso and .img disc images.
pub fn scan(root: &Path, extensions: &[String]) -> Result<Vec<PathBuf>> {
    let ext_lower: Vec<String> = extensions.iter().map(|e| e.to_lowercase()).collect();

    let mut files: Vec<PathBuf> = WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| {
            let path = entry.path();
            if iso::is_disc_image(path) {
                return true;
            }
            path.extension()
                .and_then(|e| e.to_str())
                .map(|e| ext_lower.contains(&e.to_lowercase()))
                .unwrap_or(false)
        })
        .map(|entry| entry.into_path())
        .collect();

    files.sort();

    log::info!(
        "Scanned {:?}: found {} media files",
        root,
        files.len()
    );

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
}
