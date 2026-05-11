use anyhow::Result;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::iso;

/// Filesystem types we consider "network mounts" — slow or stale mounts of
/// these types are the most common cause of indefinite hangs in `read_dir`
/// / `metadata` / ffprobe. Detection is best-effort and Linux-specific
/// (parses `/proc/mounts`); on other platforms it always returns `None`.
const NETWORK_FS_TYPES: &[&str] = &[
    "nfs",
    "nfs4",
    "smb",
    "smb2",
    "smb3",
    "smbfs",
    "cifs",
    "fuse.sshfs",
    "afs",
    "ceph",
    "9p",
    "glusterfs",
];

/// If `path` resides on a network filesystem, return the mount type name
/// (e.g. "nfs4", "cifs"). Returns `None` on non-Linux platforms or when
/// detection fails.
///
/// IMPORTANT: this is purely advisory — used to emit a "you're scanning a
/// network mount, hangs here will not be fast" warning. Do not gate any
/// logic on its result.
///
/// We deliberately avoid `path.canonicalize()` here: that does filesystem IO
/// (resolves symlinks, stats components) which itself can block indefinitely
/// on the stale-NFS scenario this function exists to warn about. Use a
/// lexical `current_dir + path` join instead — accurate enough for prefix
/// matching against `/proc/mounts` and doesn't touch the suspect mount.
pub fn detect_network_mount(path: &Path) -> Option<String> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        // current_dir() reads the process's recorded cwd from the kernel
        // (no IO on `path`), so it can't hang on the suspect mount.
        std::env::current_dir().ok()?.join(path)
    };
    let abs_str = abs.to_str()?.to_string();

    // /proc/mounts is Linux-only. Returning None on macOS/BSD is fine —
    // those hosts don't run hvac in production anyway.
    let mounts = std::fs::read_to_string("/proc/mounts").ok()?;

    // /proc/mounts lists every mount; pick the longest mount-point prefix
    // that contains `path`. That's the actual filesystem hosting the path.
    let mut best: Option<(usize, String)> = None;
    for line in mounts.lines() {
        let mut parts = line.split_whitespace();
        // Skip malformed lines instead of `?`-bailing the whole detection —
        // one bad entry shouldn't disable network-mount warnings for the
        // entire scan.
        let (_device, mount_point, fs_type) = match (parts.next(), parts.next(), parts.next()) {
            (Some(d), Some(m), Some(f)) => (d, m, f),
            _ => continue,
        };

        // "/" matches anything; other mount points match an exact path or
        // a path that descends into them (with a trailing slash to avoid
        // "/foo" matching "/foobar").
        let is_prefix = mount_point == "/"
            || abs_str == mount_point
            || abs_str.starts_with(&format!("{}/", mount_point));
        if !is_prefix {
            continue;
        }
        let len = mount_point.len();
        let replace = match &best {
            Some((b, _)) => len > *b,
            None => true,
        };
        if replace {
            best = Some((len, fs_type.to_string()));
        }
    }

    let (_, fs_type) = best?;
    if NETWORK_FS_TYPES
        .iter()
        .any(|n| fs_type == *n || fs_type.starts_with(&format!("{}.", n)))
    {
        Some(fs_type)
    } else {
        None
    }
}

/// Recursively collect all files under `dir`, following symlinks.
/// Tracks visited canonical directory paths to prevent infinite symlink cycles.
///
/// LIMITATION: `std::fs::metadata` and `std::fs::read_dir` have no timeout
/// and will block indefinitely on a stale NFS or unresponsive SMB mount.
/// Adding a timeout here requires moving the walk off-thread with a
/// watchdog, which is out of scope for the probe-timeout PR. Callers that
/// scan network mounts should be aware of this limitation; `probe_file`
/// has its own watchdog (see probe.rs::wait_with_timeout) so a hang during
/// the probe phase is bounded, but a hang during the walk phase is not.
fn walk_files(dir: &Path, out: &mut Vec<PathBuf>, visited_dirs: &mut HashSet<PathBuf>) {
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
            Ok(m) if m.is_dir() => walk_files(&path, out, visited_dirs),
            Ok(m) if m.is_file() => out.push(path),
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

    let mut all_files = Vec::new();
    walk_files(root, &mut all_files, &mut HashSet::new());

    let mut seen = HashSet::new();
    let mut files: Vec<PathBuf> = all_files
        .into_iter()
        .filter(|path| {
            // Skip hvac temporary and transcoded output files
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with(".hvac_tmp_") {
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
        fs::write(dir.path().join(".hvac_tmp_episode.mkv"), "fake").unwrap();

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
    fn test_detect_network_mount_local_paths_are_not_network() {
        // We can't blindly assert `None` — on some test hosts /tmp itself
        // is on a network mount (CI runners on shared infra, dev machines
        // with networked $TMPDIR). The contract we *can* verify is "the
        // result is in the recognized-network-fs list, or it's None".
        // Either is correct; what we want to catch is the function ever
        // returning a stray non-empty string for an unrelated fs type.
        let dir = tempfile::tempdir().unwrap();
        match detect_network_mount(dir.path()) {
            None => {}
            Some(fs) => assert!(
                NETWORK_FS_TYPES
                    .iter()
                    .any(|n| fs == *n || fs.starts_with(&format!("{}.", n))),
                "detect_network_mount returned non-network fs type: {:?}",
                fs
            ),
        }
    }

    #[test]
    fn test_detect_network_mount_handles_nonexistent_path() {
        // The lexical-join codepath doesn't touch the filesystem on the
        // input path, so missing paths just fall through /proc/mounts
        // matching. The result is whatever /proc/mounts says about that
        // ancestor — usually `/` (rootfs), so None.
        let result = detect_network_mount(Path::new("/this/path/does/not/exist/anywhere"));
        // Result must either be None or a recognized network fs, never
        // a bogus non-network string.
        if let Some(ref fs) = result {
            assert!(
                NETWORK_FS_TYPES
                    .iter()
                    .any(|n| fs == n || fs.starts_with(&format!("{}.", n))),
                "got non-network fs type: {:?}",
                fs
            );
        }
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
