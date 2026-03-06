use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Check if isomage is available on the system.
pub fn isomage_available() -> bool {
    Command::new("isomage")
        .arg("-h")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// List media files inside an ISO/IMG using isomage.
/// Returns paths relative to the disc root (e.g. "BDMV/STREAM/00000.m2ts").
pub fn list_media_files(iso_path: &Path, media_extensions: &[String]) -> Result<Vec<String>> {
    let output = Command::new("isomage")
        .arg(iso_path)
        .output()
        .context("Failed to run isomage")?;

    if !output.status.success() {
        bail!(
            "isomage failed for {:?}: {}",
            iso_path,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let ext_lower: Vec<String> = media_extensions.iter().map(|e| e.to_lowercase()).collect();

    let files: Vec<String> = stdout
        .lines()
        .map(|line| line.trim())
        // isomage output uses tree indicators like 📁 and 📄 - strip them
        .map(|line| {
            line.trim_start_matches("📁 ")
                .trim_start_matches("📄 ")
                .trim_start_matches("├── ")
                .trim_start_matches("└── ")
                .trim_start_matches("│   ")
                .trim()
                .to_string()
        })
        .filter(|line| !line.is_empty())
        .filter(|line| {
            Path::new(line)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| ext_lower.contains(&e.to_lowercase()))
                .unwrap_or(false)
        })
        .collect();

    Ok(files)
}

/// Extract a specific file from an ISO/IMG to a destination directory using isomage.
/// Returns the path to the extracted file.
pub fn extract_file(iso_path: &Path, inner_path: &str, dest_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dest_dir)?;

    let status = Command::new("isomage")
        .arg("-x")
        .arg(inner_path)
        .arg("-o")
        .arg(dest_dir)
        .arg(iso_path)
        .status()
        .context("Failed to run isomage for extraction")?;

    if !status.success() {
        bail!(
            "isomage extraction failed for {:?} from {:?}",
            inner_path,
            iso_path
        );
    }

    let extracted = dest_dir.join(inner_path);
    if !extracted.exists() {
        bail!(
            "isomage reported success but extracted file not found at {:?}",
            extracted
        );
    }

    Ok(extracted)
}

/// Returns true if the given path is an ISO or IMG disc image.
pub fn is_disc_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| matches!(e.to_lowercase().as_str(), "iso" | "img"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_disc_image() {
        assert!(is_disc_image(Path::new("/media/movie.iso")));
        assert!(is_disc_image(Path::new("/media/movie.ISO")));
        assert!(is_disc_image(Path::new("/media/movie.img")));
        assert!(is_disc_image(Path::new("/media/movie.IMG")));
        assert!(!is_disc_image(Path::new("/media/movie.mkv")));
        assert!(!is_disc_image(Path::new("/media/movie.mp4")));
    }
}
