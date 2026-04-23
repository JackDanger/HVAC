use anyhow::{bail, Context, Result};
use std::path::Path;

/// Returns available disk space in bytes for the filesystem containing `path`.
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
    fn test_available_disk_space() {
        // Just verify the call succeeds — actual value depends on host
        let _space = available_disk_space(Path::new("/")).unwrap();
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(1024 * 1024), "1MB");
        assert_eq!(format_size(500 * 1024 * 1024), "500MB");
        assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2.0GB");
        assert_eq!(format_size(512 * 1024), "512KB");
    }
}
