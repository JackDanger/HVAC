use anyhow::{bail, Context, Result};
use std::path::Path;

/// Returns available disk space in bytes for the filesystem containing `path`.
///
/// Uses `statvfs(2)`. Returns the bytes available to a non-root caller
/// (`f_bavail × f_frsize`), not the raw free space — i.e. it accounts for
/// filesystem reserves the way the user would expect.
pub fn available_disk_space(path: &Path) -> Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).context("Path contains null bytes")?;

    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut stat) != 0 {
            bail!(
                "Failed to check disk space: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(stat.f_bavail as u64 * stat.f_frsize)
    }
}

/// Format a byte count for human display. Picks the largest unit that
/// gives a value ≥ 1, then chooses precision:
///
/// - **GB**: one decimal (`"2.0GB"`, `"3.5GB"`).
/// - **MB**: integer (`"500MB"`).
/// - **KB**: integer; also the unit for anything < 1 MB including 0.
///
/// Uses binary units (`1024`-based) — what `du -h` and the rest of the
/// system tools speak. Bytes < 1024 are rendered as a fractional KB
/// (`"0KB"` for 0, `"0KB"` for 100 — sub-KB resolution is not useful
/// for the things this function is called on).
pub fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1}GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.0}MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.0}KB", bytes as f64 / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_disk_space_root_returns_a_value() {
        // Just verify the call succeeds. Every supported platform has `/`.
        let space = available_disk_space(Path::new("/")).unwrap();
        // Sanity: the root fs has *some* space. A box with literally 0
        // bytes free would have failed many earlier syscalls.
        assert!(space > 0, "root filesystem reported 0 bytes free");
    }

    #[test]
    fn available_disk_space_path_with_null_byte_errors() {
        // CString::new rejects interior nulls; we surface that as a clean
        // error instead of crashing.
        use std::os::unix::ffi::OsStrExt;
        let bad = std::ffi::OsStr::from_bytes(b"/tmp\0/x");
        let err = available_disk_space(Path::new(bad)).unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("null"),
            "expected null-byte error, got: {}",
            err
        );
    }

    #[test]
    fn format_size_basic_units() {
        assert_eq!(format_size(1024 * 1024), "1MB");
        assert_eq!(format_size(500 * 1024 * 1024), "500MB");
        assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2.0GB");
        assert_eq!(format_size(512 * 1024), "512KB");
    }

    #[test]
    fn format_size_zero_renders_zero_kb() {
        assert_eq!(format_size(0), "0KB");
    }

    #[test]
    fn format_size_sub_kb_rounds_to_zero_kb() {
        // < 1024 bytes is below our display resolution. We still want a
        // consistent "0KB" rather than "0.X KB" so tabular output aligns.
        assert_eq!(format_size(100), "0KB");
        assert_eq!(format_size(1023), "1KB"); // 1023 / 1024 = 0.999 → rounds to 1 with "{:.0}"
    }

    #[test]
    fn format_size_boundary_between_mb_and_gb() {
        // Exactly 1024 MB = 1 GB → renders as GB, one decimal.
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0GB");
        // Just below the boundary stays MB.
        assert_eq!(format_size(1024 * 1024 * 1024 - 1), "1024MB");
    }

    #[test]
    fn format_size_boundary_between_kb_and_mb() {
        // Exactly 1024 KB = 1 MB.
        assert_eq!(format_size(1024 * 1024), "1MB");
        // Just below.
        assert_eq!(format_size(1024 * 1024 - 1), "1024KB");
    }

    #[test]
    fn format_size_large_values_still_in_gb() {
        // Once you cross 1 GB we never switch to TB — Tdarr-replacement
        // libraries top out at a few TB and "1500GB" reads fine.
        assert_eq!(format_size(1500u64 * 1024 * 1024 * 1024), "1500.0GB");
    }

    #[test]
    fn format_size_u64_max_does_not_panic() {
        // Defensive: never panic on absurd input (some FFmpeg / statvfs
        // edge cases can report bizarre values). Just doesn't crash.
        let _ = format_size(u64::MAX);
    }
}
