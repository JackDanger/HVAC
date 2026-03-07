use anyhow::{Context, Result};
use std::fs::File;
use std::io::Write;
use std::path::Path;

/// Returns true if the given path is an ISO or IMG disc image.
pub fn is_disc_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| matches!(e.to_lowercase().as_str(), "iso" | "img"))
        .unwrap_or(false)
}

/// List media files inside an ISO/IMG using the isomage library.
/// Returns paths relative to the disc root (e.g. "STREAM/00000.M2T").
pub fn list_media_files(iso_path: &Path, media_extensions: &[String]) -> Result<Vec<String>> {
    let mut file = File::open(iso_path)
        .with_context(|| format!("Failed to open {:?}", iso_path))?;
    let filename = iso_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let root = isomage::detect_and_parse_filesystem(&mut file, &filename)
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    let ext_lower: Vec<String> = media_extensions.iter().map(|e| e.to_lowercase()).collect();
    let mut results = Vec::new();
    collect_media_files(&root, "", &ext_lower, &mut results);
    Ok(results)
}

fn collect_media_files(
    node: &isomage::TreeNode,
    prefix: &str,
    exts: &[String],
    results: &mut Vec<String>,
) {
    for child in &node.children {
        let path = if prefix.is_empty() {
            child.name.clone()
        } else {
            format!("{}/{}", prefix, child.name)
        };
        if child.is_directory {
            collect_media_files(child, &path, exts, results);
        } else if has_media_extension(&child.name, exts) {
            results.push(path);
        }
    }
}

fn has_media_extension(name: &str, exts: &[String]) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| exts.contains(&e.to_lowercase()))
        .unwrap_or(false)
}

/// Stream a file from inside an ISO to a writer using the isomage library.
/// This avoids extracting to disk — data goes directly to the writer (e.g. ffmpeg stdin).
pub fn cat_file<W: Write>(iso_path: &Path, inner_path: &str, writer: &mut W) -> Result<()> {
    let mut file = File::open(iso_path)
        .with_context(|| format!("Failed to open {:?}", iso_path))?;
    let filename = iso_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let root = isomage::detect_and_parse_filesystem(&mut file, &filename)
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    let node = root
        .find_node(inner_path)
        .ok_or_else(|| anyhow::anyhow!("File not found in ISO: {}", inner_path))?;

    isomage::cat_node(&mut file, node, writer).map_err(|e| anyhow::anyhow!("{}", e))
}

/// Get the size of a file inside an ISO without extracting it.
pub fn file_size(iso_path: &Path, inner_path: &str) -> Result<u64> {
    let mut file = File::open(iso_path)
        .with_context(|| format!("Failed to open {:?}", iso_path))?;
    let filename = iso_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let root = isomage::detect_and_parse_filesystem(&mut file, &filename)
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    let node = root
        .find_node(inner_path)
        .ok_or_else(|| anyhow::anyhow!("File not found in ISO: {}", inner_path))?;

    Ok(node.size)
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

    #[test]
    fn test_list_media_files_in_bdmv_disc() {
        let iso_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/BDMV_DISC.iso");
        if !iso_path.exists() {
            eprintln!("Skipping: test fixture not found");
            return;
        }
        let exts = vec!["m2ts".to_string(), "m2t".to_string(), "mkv".to_string()];
        let files = list_media_files(&iso_path, &exts).expect("Failed to list media files");
        assert!(!files.is_empty(), "Should find media files in BDMV disc");
        assert!(
            files.iter().any(|f| f.contains("00000")),
            "Should find 00000.M2T(S): {:?}",
            files
        );
    }

    #[test]
    fn test_cat_file_from_bdmv_disc() {
        let iso_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/BDMV_DISC.iso");
        if !iso_path.exists() {
            eprintln!("Skipping: test fixture not found");
            return;
        }
        let exts = vec!["m2ts".to_string(), "m2t".to_string()];
        let files = list_media_files(&iso_path, &exts).unwrap();
        let inner = &files[0];

        // Read first 188 bytes (one MPEG-TS packet)
        let mut buf = Vec::new();
        cat_file(&iso_path, inner, &mut buf).expect("cat_file failed");

        // MPEG-TS sync byte is 0x47
        assert!(buf.len() > 188, "Should have at least one TS packet");
        // Find sync byte — UDF/ISO preamble may offset it slightly
        let has_sync = buf.windows(1).any(|w| w[0] == 0x47);
        assert!(has_sync, "Should contain at least one MPEG-TS sync byte (0x47)");
    }

    #[test]
    fn test_file_size_from_bdmv_disc() {
        let iso_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/BDMV_DISC.iso");
        if !iso_path.exists() {
            eprintln!("Skipping: test fixture not found");
            return;
        }
        let exts = vec!["m2ts".to_string(), "m2t".to_string()];
        let files = list_media_files(&iso_path, &exts).unwrap();
        let inner = &files[0];

        let size = file_size(&iso_path, inner).expect("file_size failed");
        assert!(size > 1_000_000, "File should be at least 1MB, got {}", size);
    }
}
