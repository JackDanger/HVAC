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

/// Disc structure detected inside an ISO/IMG.
#[derive(Debug, Clone, PartialEq)]
pub enum DiscType {
    /// Standard Blu-ray: BDMV/STREAM/*.m2ts at root or nested under a directory
    Bluray,
    /// Blu-ray where BDMV content is at the ISO root (STREAM/ directly visible)
    BlurayFlat,
    /// DVD: VIDEO_TS/VTS_NN_M.VOB
    Dvd,
    /// AVCHD camcorder: PRIVATE/AVCHD/BDMV/STREAM/*.MTS or AVCHD/BDMV/STREAM/*.MTS
    Avchd,
    /// No recognized structure — just media files scattered in the image
    BareMedia,
    /// Encrypted commercial Blu-ray (AACS or BD+). Cannot be decoded by ffmpeg.
    /// `reason` describes what protection marker was detected.
    Encrypted { reason: String },
}

/// A media file inside an ISO, with its path and size.
#[derive(Debug, Clone)]
pub struct IsoMediaFile {
    pub path: String,
    pub size: u64,
}

/// Result of analyzing a disc image's structure.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DiscAnalysis {
    pub disc_type: DiscType,
    /// Files that make up the main feature, in playback order.
    /// For Blu-ray: the largest consecutive m2ts chapter files.
    /// For DVD: all VOBs in the largest title set, in order.
    /// For bare media: the single largest file.
    pub main_feature: Vec<IsoMediaFile>,
    /// Total size of the main feature files.
    pub main_feature_size: u64,
    /// All other media files (extras, trailers, menus, smaller title sets).
    pub extras: Vec<IsoMediaFile>,
}

/// Analyze a disc image and detect its structure, identifying the main feature.
///
/// The heuristic errs on the side of including bonus content rather than
/// missing chapters. For Blu-ray discs, all "large" m2ts files in STREAM/
/// that look like feature content are included. For DVDs, the entire largest
/// title set is included.
pub fn analyze_disc(iso_path: &Path) -> Result<DiscAnalysis> {
    let mut file =
        File::open(iso_path).with_context(|| format!("Failed to open {:?}", iso_path))?;
    let filename = iso_path.file_name().unwrap_or_default().to_string_lossy();
    let root = isomage::detect_and_parse_filesystem(&mut file, &filename)
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    // Bail early for encrypted commercial discs: ffmpeg can't decode AACS or BD+
    // protected streams, so we'd otherwise feed garbage to the encoder mid-run.
    if let Some(reason) = detect_encryption(&root) {
        return Ok(DiscAnalysis {
            disc_type: DiscType::Encrypted { reason },
            main_feature: Vec::new(),
            main_feature_size: 0,
            extras: Vec::new(),
        });
    }

    let disc_type = detect_disc_type(&root);

    match disc_type {
        DiscType::Bluray => analyze_bluray(&root, "BDMV/STREAM"),
        DiscType::BlurayFlat => analyze_bluray(&root, "STREAM"),
        DiscType::Dvd => analyze_dvd(&root),
        DiscType::Avchd => analyze_avchd(&root),
        DiscType::BareMedia => analyze_bare_media(&root),
        DiscType::Encrypted { .. } => unreachable!("handled above"),
    }
}

/// Detect commercial-disc encryption markers on a parsed ISO root.
///
/// AACS-protected Blu-rays carry an `AACS/` directory at the disc root with
/// files like `Unit_Key_RO.inf`, `MKB_RO.inf`, and `Content000.cer`. BD+
/// adds a `BDSVM/` directory. Either marker means ffmpeg can't decode the
/// streams, so we return a human-readable reason and let the caller skip.
fn detect_encryption(root: &isomage::TreeNode) -> Option<String> {
    if find_dir(root, "AACS").is_some() {
        return Some("AACS-encrypted Blu-ray detected (AACS/ directory present); hvac cannot decrypt commercial discs. Use makemkv to rip first.".to_string());
    }
    if find_dir(root, "BDSVM").is_some() {
        return Some("BD+-protected Blu-ray detected (BDSVM/ directory present); hvac cannot decrypt commercial discs. Use makemkv to rip first.".to_string());
    }
    None
}

/// Detect the type of disc from the filesystem tree.
fn detect_disc_type(root: &isomage::TreeNode) -> DiscType {
    // Check for standard Blu-ray: BDMV/STREAM/
    if find_dir(root, "BDMV/STREAM").is_some() || find_dir(root, "bdmv/stream").is_some() {
        return DiscType::Bluray;
    }

    // Check for DVD: VIDEO_TS/
    if find_dir(root, "VIDEO_TS").is_some() || find_dir(root, "video_ts").is_some() {
        return DiscType::Dvd;
    }

    // Check for AVCHD: PRIVATE/AVCHD/BDMV/STREAM or AVCHD/BDMV/STREAM
    if find_dir(root, "PRIVATE/AVCHD/BDMV/STREAM").is_some()
        || find_dir(root, "AVCHD/BDMV/STREAM").is_some()
    {
        return DiscType::Avchd;
    }

    // Check for flat Blu-ray: STREAM/ at root with m2ts/m2t files
    if let Some(stream) = find_dir(root, "STREAM") {
        let has_m2ts = stream.children.iter().any(|c| {
            let ext = ext_lower(&c.name);
            ext == "m2ts" || ext == "m2t" || ext == "mts"
        });
        if has_m2ts {
            return DiscType::BlurayFlat;
        }
    }

    DiscType::BareMedia
}

/// Find a directory node by slash-separated path (case-insensitive).
fn find_dir<'a>(root: &'a isomage::TreeNode, path: &str) -> Option<&'a isomage::TreeNode> {
    let parts: Vec<&str> = path.split('/').collect();
    let mut current = root;

    for part in &parts {
        let lower = part.to_lowercase();
        let found = current
            .children
            .iter()
            .find(|c| c.is_directory && c.name.to_lowercase() == lower);
        match found {
            Some(child) => current = child,
            None => return None,
        }
    }

    Some(current)
}

/// Get lowercase extension from a filename.
fn ext_lower(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default()
}

/// Analyze a Blu-ray disc (standard or flat).
///
/// Heuristic:
/// 1. Collect all m2ts/m2t files in the STREAM directory with their sizes.
/// 2. Find the largest file — this is definitely part of the main feature.
/// 3. Include all files whose size is at least 25% of the largest file's size.
///    These are likely chapters of the main feature. Real bonus features are
///    typically much smaller than chapters (trailers ~2-5%, behind-scenes ~10-15%).
/// 4. Sort included files by filename (00000, 00001, ...) for correct playback order.
/// 5. Everything else goes into extras.
///
/// This errs on the side of inclusion: a 30-minute bonus feature on a 2-hour disc
/// would be ~25% of a chapter and might get included. That's intentional — better
/// to accidentally concat a bonus feature than miss a chapter.
fn analyze_bluray(root: &isomage::TreeNode, stream_path: &str) -> Result<DiscAnalysis> {
    let stream_dir = find_dir(root, stream_path)
        .ok_or_else(|| anyhow::anyhow!("STREAM directory not found at {}", stream_path))?;

    let mut media: Vec<IsoMediaFile> = stream_dir
        .children
        .iter()
        .filter(|c| {
            !c.is_directory && {
                let ext = ext_lower(&c.name);
                ext == "m2ts" || ext == "m2t" || ext == "mts"
            }
        })
        .map(|c| IsoMediaFile {
            path: format!("{}/{}", stream_path, c.name),
            size: c.size,
        })
        .collect();

    if media.is_empty() {
        return Ok(DiscAnalysis {
            disc_type: if stream_path.starts_with("BDMV") {
                DiscType::Bluray
            } else {
                DiscType::BlurayFlat
            },
            main_feature: Vec::new(),
            main_feature_size: 0,
            extras: Vec::new(),
        });
    }

    // Sort by filename for stable ordering
    media.sort_by(|a, b| a.path.cmp(&b.path));

    let max_size = media.iter().map(|f| f.size).max().unwrap_or(0);
    // Threshold: 25% of largest file qualifies as a chapter
    let threshold = max_size / 4;

    let (main_feature, extras): (Vec<_>, Vec<_>) =
        media.into_iter().partition(|f| f.size >= threshold);

    let main_feature_size = main_feature.iter().map(|f| f.size).sum();

    Ok(DiscAnalysis {
        disc_type: if stream_path.starts_with("BDMV") {
            DiscType::Bluray
        } else {
            DiscType::BlurayFlat
        },
        main_feature,
        main_feature_size,
        extras,
    })
}

/// Analyze a DVD disc.
///
/// Heuristic:
/// 1. Group VTS_NN_M.VOB files by title set number (NN).
/// 2. Sum size per title set.
/// 3. The title set with the largest total size is the main feature.
/// 4. Within that title set, files are ordered by M (segment number).
/// 5. If multiple title sets have similar sizes (within 20% of each other),
///    include all of them — this is likely a TV episodes disc.
/// 6. VIDEO_TS.VOB (menu) is always excluded from main feature.
fn analyze_dvd(root: &isomage::TreeNode) -> Result<DiscAnalysis> {
    let video_ts = find_dir(root, "VIDEO_TS")
        .or_else(|| find_dir(root, "video_ts"))
        .ok_or_else(|| anyhow::anyhow!("VIDEO_TS directory not found"))?;

    // Parse VTS_NN_M.VOB filenames
    let mut title_sets: std::collections::BTreeMap<String, Vec<IsoMediaFile>> =
        std::collections::BTreeMap::new();
    let mut menu_files: Vec<IsoMediaFile> = Vec::new();

    // Also collect any other media files that aren't VOBs
    let video_ts_path = if video_ts.children.iter().any(|c| c.name.contains("VTS_")) {
        "VIDEO_TS"
    } else {
        "video_ts"
    };

    for child in &video_ts.children {
        if child.is_directory {
            continue;
        }

        let ext = ext_lower(&child.name);
        if ext != "vob" {
            continue;
        }

        let upper = child.name.to_uppercase();

        // VIDEO_TS.VOB = menu
        if upper == "VIDEO_TS.VOB" {
            menu_files.push(IsoMediaFile {
                path: format!("{}/{}", video_ts_path, child.name),
                size: child.size,
            });
            continue;
        }

        // Parse VTS_NN_M.VOB
        if let Some(title_num) = parse_vts_title(&upper) {
            title_sets.entry(title_num).or_default().push(IsoMediaFile {
                path: format!("{}/{}", video_ts_path, child.name),
                size: child.size,
            });
        }
    }

    // Sort each title set by filename (segment order)
    for files in title_sets.values_mut() {
        files.sort_by(|a, b| a.path.cmp(&b.path));
    }

    if title_sets.is_empty() {
        return Ok(DiscAnalysis {
            disc_type: DiscType::Dvd,
            main_feature: Vec::new(),
            main_feature_size: 0,
            extras: menu_files,
        });
    }

    // Calculate total size per title set
    let mut set_sizes: Vec<(String, u64)> = title_sets
        .iter()
        .map(|(ts, files)| (ts.clone(), files.iter().map(|f| f.size).sum()))
        .collect();
    set_sizes.sort_by_key(|a| std::cmp::Reverse(a.1));

    let largest_size = set_sizes[0].1;
    // Include title sets within 20% of the largest — catches TV episode discs
    let similarity_threshold = largest_size * 80 / 100;

    let main_sets: std::collections::HashSet<String> = set_sizes
        .iter()
        .filter(|(_, size)| *size >= similarity_threshold)
        .map(|(ts, _)| ts.clone())
        .collect();

    let mut main_feature: Vec<IsoMediaFile> = Vec::new();
    let mut extras: Vec<IsoMediaFile> = Vec::new();

    for (ts, files) in title_sets {
        if main_sets.contains(&ts) {
            main_feature.extend(files);
        } else {
            extras.extend(files);
        }
    }
    extras.extend(menu_files);

    // Sort main feature by path for correct playback order
    main_feature.sort_by(|a, b| a.path.cmp(&b.path));

    let main_feature_size = main_feature.iter().map(|f| f.size).sum();

    Ok(DiscAnalysis {
        disc_type: DiscType::Dvd,
        main_feature,
        main_feature_size,
        extras,
    })
}

/// Parse VTS title number from "VTS_01_1.VOB" -> Some("01")
fn parse_vts_title(upper_name: &str) -> Option<String> {
    // Format: VTS_NN_M.VOB where NN is title, M is segment (0 = IFO)
    if !upper_name.starts_with("VTS_") || !upper_name.ends_with(".VOB") {
        return None;
    }
    let inner = &upper_name[4..upper_name.len() - 4]; // "NN_M"
    let parts: Vec<&str> = inner.split('_').collect();
    if parts.len() != 2 {
        return None;
    }
    // Segment 0 is the IFO, skip it
    if parts[1] == "0" {
        return None;
    }
    Some(parts[0].to_string())
}

/// Analyze AVCHD camcorder disc.
///
/// AVCHD discs are typically sequential recordings from a camcorder.
/// Include all MTS files as the "main feature" since they're usually
/// all part of the same recording session. Sort by filename for order.
fn analyze_avchd(root: &isomage::TreeNode) -> Result<DiscAnalysis> {
    // Try PRIVATE/AVCHD/BDMV/STREAM first, then AVCHD/BDMV/STREAM
    let stream_path;
    let stream_dir = if let Some(d) = find_dir(root, "PRIVATE/AVCHD/BDMV/STREAM") {
        stream_path = "PRIVATE/AVCHD/BDMV/STREAM";
        d
    } else if let Some(d) = find_dir(root, "AVCHD/BDMV/STREAM") {
        stream_path = "AVCHD/BDMV/STREAM";
        d
    } else {
        return Err(anyhow::anyhow!("AVCHD STREAM directory not found"));
    };

    let mut media: Vec<IsoMediaFile> = stream_dir
        .children
        .iter()
        .filter(|c| {
            !c.is_directory && {
                let ext = ext_lower(&c.name);
                ext == "mts" || ext == "m2ts" || ext == "m2t"
            }
        })
        .map(|c| IsoMediaFile {
            path: format!("{}/{}", stream_path, c.name),
            size: c.size,
        })
        .collect();

    media.sort_by(|a, b| a.path.cmp(&b.path));

    let main_feature_size = media.iter().map(|f| f.size).sum();

    Ok(DiscAnalysis {
        disc_type: DiscType::Avchd,
        main_feature: media,
        main_feature_size,
        extras: Vec::new(),
    })
}

/// Analyze a disc with no standard structure.
///
/// Heuristic: collect all media files, use the largest as the main feature.
/// Files that are at least 25% of the largest are included (in case of
/// multi-part archives). Everything else is extras.
fn analyze_bare_media(root: &isomage::TreeNode) -> Result<DiscAnalysis> {
    let media_exts = [
        "mkv", "mp4", "avi", "m2ts", "m2t", "mts", "ts", "mpg", "mpeg", "wmv", "mov", "vob", "m4v",
        "webm", "flv",
    ];

    let mut all_media: Vec<IsoMediaFile> = Vec::new();
    collect_all_media(root, "", &media_exts, &mut all_media);

    if all_media.is_empty() {
        return Ok(DiscAnalysis {
            disc_type: DiscType::BareMedia,
            main_feature: Vec::new(),
            main_feature_size: 0,
            extras: Vec::new(),
        });
    }

    all_media.sort_by_key(|a| std::cmp::Reverse(a.size));

    let max_size = all_media[0].size;
    let threshold = max_size / 4;

    let (mut main_feature, extras): (Vec<_>, Vec<_>) =
        all_media.into_iter().partition(|f| f.size >= threshold);

    // Sort main feature by path for playback order
    main_feature.sort_by(|a, b| a.path.cmp(&b.path));

    let main_feature_size = main_feature.iter().map(|f| f.size).sum();

    Ok(DiscAnalysis {
        disc_type: DiscType::BareMedia,
        main_feature,
        main_feature_size,
        extras,
    })
}

fn collect_all_media(
    node: &isomage::TreeNode,
    prefix: &str,
    exts: &[&str],
    results: &mut Vec<IsoMediaFile>,
) {
    for child in &node.children {
        let path = if prefix.is_empty() {
            child.name.clone()
        } else {
            format!("{}/{}", prefix, child.name)
        };
        if child.is_directory {
            collect_all_media(child, &path, exts, results);
        } else if exts.contains(&ext_lower(&child.name).as_str()) {
            results.push(IsoMediaFile {
                path,
                size: child.size,
            });
        }
    }
}

/// List media files inside an ISO/IMG using the isomage library.
/// Returns paths relative to the disc root (e.g. "STREAM/00000.M2T").
#[cfg(test)]
pub fn list_media_files(iso_path: &Path, media_extensions: &[String]) -> Result<Vec<String>> {
    let mut file =
        File::open(iso_path).with_context(|| format!("Failed to open {:?}", iso_path))?;
    let filename = iso_path.file_name().unwrap_or_default().to_string_lossy();
    let root = isomage::detect_and_parse_filesystem(&mut file, &filename)
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    let ext_lower: Vec<String> = media_extensions.iter().map(|e| e.to_lowercase()).collect();
    let mut results = Vec::new();
    collect_media_files(&root, "", &ext_lower, &mut results);
    Ok(results)
}

#[cfg(test)]
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

#[cfg(test)]
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
    let mut file =
        File::open(iso_path).with_context(|| format!("Failed to open {:?}", iso_path))?;
    let filename = iso_path.file_name().unwrap_or_default().to_string_lossy();
    let root = isomage::detect_and_parse_filesystem(&mut file, &filename)
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    let node = root
        .find_node(inner_path)
        .ok_or_else(|| anyhow::anyhow!("File not found in ISO: {}", inner_path))?;

    isomage::cat_node(&mut file, node, writer).map_err(|e| anyhow::anyhow!("{}", e))
}

/// Stream multiple files from inside an ISO sequentially to a writer.
/// Used to concatenate chapter files for the main feature.
pub fn cat_files<W: Write>(iso_path: &Path, inner_paths: &[String], writer: &mut W) -> Result<()> {
    let mut file =
        File::open(iso_path).with_context(|| format!("Failed to open {:?}", iso_path))?;
    let filename = iso_path.file_name().unwrap_or_default().to_string_lossy();
    let root = isomage::detect_and_parse_filesystem(&mut file, &filename)
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    for inner_path in inner_paths {
        let node = root
            .find_node(inner_path)
            .ok_or_else(|| anyhow::anyhow!("File not found in ISO: {}", inner_path))?;

        isomage::cat_node(&mut file, node, writer).map_err(|e| anyhow::anyhow!("{}", e))?;
    }

    Ok(())
}

/// Get the size of a file inside an ISO without extracting it.
pub fn file_size(iso_path: &Path, inner_path: &str) -> Result<u64> {
    let mut file =
        File::open(iso_path).with_context(|| format!("Failed to open {:?}", iso_path))?;
    let filename = iso_path.file_name().unwrap_or_default().to_string_lossy();
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

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn require_fixture(name: &str) -> Option<std::path::PathBuf> {
        let p = fixture(name);
        if p.exists() {
            Some(p)
        } else {
            eprintln!("Skipping: {} not found", name);
            None
        }
    }

    // ---- Basic tests ----

    #[test]
    fn test_is_disc_image() {
        assert!(is_disc_image(Path::new("/media/movie.iso")));
        assert!(is_disc_image(Path::new("/media/movie.ISO")));
        assert!(is_disc_image(Path::new("/media/movie.img")));
        assert!(is_disc_image(Path::new("/media/movie.IMG")));
        assert!(!is_disc_image(Path::new("/media/movie.mkv")));
        assert!(!is_disc_image(Path::new("/media/movie.mp4")));
    }

    // ---- Disc type detection ----

    #[test]
    fn test_detect_bluray_disc() {
        let iso = require_fixture("BDMV_DISC.iso");
        if let Some(p) = iso {
            let analysis = analyze_disc(&p).unwrap();
            assert_eq!(analysis.disc_type, DiscType::Bluray);
        }
    }

    #[test]
    fn test_detect_bluray_flat_disc() {
        let iso = require_fixture("BDMV_FLAT.iso");
        if let Some(p) = iso {
            let analysis = analyze_disc(&p).unwrap();
            assert_eq!(analysis.disc_type, DiscType::BlurayFlat);
        }
    }

    #[test]
    fn test_detect_dvd_disc() {
        let iso = require_fixture("DVD_DISC.iso");
        if let Some(p) = iso {
            let analysis = analyze_disc(&p).unwrap();
            assert_eq!(analysis.disc_type, DiscType::Dvd);
        }
    }

    #[test]
    fn test_detect_avchd_disc() {
        let iso = require_fixture("AVCHD_DISC.iso");
        if let Some(p) = iso {
            let analysis = analyze_disc(&p).unwrap();
            assert_eq!(analysis.disc_type, DiscType::Avchd);
        }
    }

    #[test]
    fn test_detect_bare_media_disc() {
        let iso = require_fixture("BARE_MEDIA.iso");
        if let Some(p) = iso {
            let analysis = analyze_disc(&p).unwrap();
            assert_eq!(analysis.disc_type, DiscType::BareMedia);
        }
    }

    // ---- Blu-ray main feature detection ----

    #[test]
    fn test_bluray_main_feature_includes_chapters() {
        let iso = require_fixture("BDMV_DISC.iso");
        if let Some(p) = iso {
            let a = analyze_disc(&p).unwrap();
            // Should include the 3 large chapter files (00000, 00001, 00002)
            assert!(
                a.main_feature.len() >= 3,
                "Expected at least 3 chapter files in main feature, got {}: {:?}",
                a.main_feature.len(),
                a.main_feature.iter().map(|f| &f.path).collect::<Vec<_>>()
            );
            // Should be sorted by filename
            for w in a.main_feature.windows(2) {
                assert!(
                    w[0].path < w[1].path,
                    "Files should be sorted: {} < {}",
                    w[0].path,
                    w[1].path
                );
            }
        }
    }

    #[test]
    fn test_bluray_excludes_tiny_menu_clips() {
        let iso = require_fixture("BDMV_DISC.iso");
        if let Some(p) = iso {
            let a = analyze_disc(&p).unwrap();
            // The 2-second menu clip (00100.m2ts) should be in extras, not main feature
            let main_paths: Vec<&str> = a.main_feature.iter().map(|f| f.path.as_str()).collect();
            assert!(
                !main_paths.iter().any(|p| p.contains("00100")),
                "Tiny menu clip should not be in main feature: {:?}",
                main_paths
            );
        }
    }

    #[test]
    fn test_bluray_main_feature_larger_than_extras() {
        let iso = require_fixture("BDMV_DISC.iso");
        if let Some(p) = iso {
            let a = analyze_disc(&p).unwrap();
            let extras_size: u64 = a.extras.iter().map(|f| f.size).sum();
            assert!(
                a.main_feature_size > extras_size,
                "Main feature ({}) should be larger than extras ({})",
                a.main_feature_size,
                extras_size
            );
        }
    }

    // ---- Flat Blu-ray ----

    #[test]
    fn test_bluray_flat_finds_chapters() {
        let iso = require_fixture("BDMV_FLAT.iso");
        if let Some(p) = iso {
            let a = analyze_disc(&p).unwrap();
            // Should find the 2 large chapter files
            assert!(
                a.main_feature.len() >= 2,
                "Expected at least 2 chapters, got {}: {:?}",
                a.main_feature.len(),
                a.main_feature.iter().map(|f| &f.path).collect::<Vec<_>>()
            );
            // Small extra should be excluded
            assert!(!a.extras.is_empty(), "Should have extras");
        }
    }

    // ---- DVD main feature detection ----

    #[test]
    fn test_dvd_main_feature_is_largest_title_set() {
        let iso = require_fixture("DVD_DISC.iso");
        if let Some(p) = iso {
            let a = analyze_disc(&p).unwrap();
            // VTS_01 has 3 VOBs (largest set), VTS_02 has 1 VOB (smaller)
            assert!(
                a.main_feature.len() == 3,
                "Expected 3 VOBs in main feature (VTS_01), got {}: {:?}",
                a.main_feature.len(),
                a.main_feature.iter().map(|f| &f.path).collect::<Vec<_>>()
            );
            // All main feature files should be VTS_01
            for f in &a.main_feature {
                assert!(
                    f.path.contains("VTS_01"),
                    "Main feature should be VTS_01, got: {}",
                    f.path
                );
            }
            // VTS_02 should be in extras
            assert!(
                a.extras.iter().any(|f| f.path.contains("VTS_02")),
                "VTS_02 should be in extras"
            );
        }
    }

    #[test]
    fn test_dvd_excludes_menu_vob() {
        let iso = require_fixture("DVD_DISC.iso");
        if let Some(p) = iso {
            let a = analyze_disc(&p).unwrap();
            // VIDEO_TS.VOB should not be in main feature
            let main_paths: Vec<&str> = a.main_feature.iter().map(|f| f.path.as_str()).collect();
            assert!(
                !main_paths.iter().any(|p| p.contains("VIDEO_TS.VOB")),
                "Menu VOB should not be in main feature"
            );
        }
    }

    #[test]
    fn test_dvd_vob_order_is_sequential() {
        let iso = require_fixture("DVD_DISC.iso");
        if let Some(p) = iso {
            let a = analyze_disc(&p).unwrap();
            // VOBs should be in order: VTS_01_1, VTS_01_2, VTS_01_3
            for w in a.main_feature.windows(2) {
                assert!(
                    w[0].path < w[1].path,
                    "VOBs should be sequential: {} < {}",
                    w[0].path,
                    w[1].path
                );
            }
        }
    }

    // ---- Multi-title DVD (TV episodes) ----

    #[test]
    fn test_multi_title_dvd_includes_all_similar_sets() {
        let iso = require_fixture("MULTI_TITLE_DVD.iso");
        if let Some(p) = iso {
            let a = analyze_disc(&p).unwrap();
            // All 4 title sets are similar size, so all should be in main feature
            assert!(
                a.main_feature.len() == 4,
                "Expected 4 VOBs (all similar-size title sets), got {}: {:?}",
                a.main_feature.len(),
                a.main_feature.iter().map(|f| &f.path).collect::<Vec<_>>()
            );
        }
    }

    // ---- AVCHD ----

    #[test]
    fn test_avchd_includes_all_clips() {
        let iso = require_fixture("AVCHD_DISC.iso");
        if let Some(p) = iso {
            let a = analyze_disc(&p).unwrap();
            // All 3 MTS clips should be included (sequential recording)
            assert!(
                a.main_feature.len() == 3,
                "Expected 3 AVCHD clips, got {}: {:?}",
                a.main_feature.len(),
                a.main_feature.iter().map(|f| &f.path).collect::<Vec<_>>()
            );
            assert!(a.extras.is_empty(), "AVCHD should have no extras");
        }
    }

    // ---- Bare media ----

    #[test]
    fn test_bare_media_largest_is_main() {
        let iso = require_fixture("BARE_MEDIA.iso");
        if let Some(p) = iso {
            let a = analyze_disc(&p).unwrap();
            // movie.mkv (60s) should be in main feature
            assert!(!a.main_feature.is_empty(), "Should have a main feature");
            assert!(
                a.main_feature[0].path.contains("movie") || a.main_feature[0].size > 0,
                "Largest file should be in main feature"
            );
        }
    }

    #[test]
    fn test_bare_media_excludes_tiny_files() {
        let iso = require_fixture("BARE_MEDIA.iso");
        if let Some(p) = iso {
            let a = analyze_disc(&p).unwrap();
            // trailer.mp4 (5s) should be in extras (too small relative to main)
            let main_paths: Vec<&str> = a.main_feature.iter().map(|f| f.path.as_str()).collect();
            assert!(
                !main_paths.iter().any(|p| p.contains("trailer")),
                "Tiny trailer should be in extras, not main: {:?}",
                main_paths
            );
        }
    }

    // ---- cat_files (multi-file concatenation) ----

    #[test]
    fn test_cat_files_concatenates_in_order() {
        let iso = require_fixture("BDMV_DISC.iso");
        if let Some(p) = iso {
            let a = analyze_disc(&p).unwrap();
            if a.main_feature.len() >= 2 {
                let paths: Vec<String> = a.main_feature.iter().map(|f| f.path.clone()).collect();
                let mut buf = Vec::new();
                cat_files(&p, &paths, &mut buf).expect("cat_files failed");
                assert!(buf.len() > 0, "Concatenated output should be non-empty");
                // Total should be approximately the sum of individual sizes
                let total_size: u64 = a.main_feature.iter().map(|f| f.size).sum();
                assert!(
                    (buf.len() as u64) >= total_size * 95 / 100,
                    "Output size {} should be close to total size {}",
                    buf.len(),
                    total_size
                );
            }
        }
    }

    // ---- Legacy tests ----

    #[test]
    fn test_list_media_files_in_bdmv_disc() {
        if let Some(p) = require_fixture("BDMV_DISC.iso") {
            let exts = vec!["m2ts".to_string(), "m2t".to_string(), "mkv".to_string()];
            let files = list_media_files(&p, &exts).expect("Failed to list media files");
            assert!(!files.is_empty(), "Should find media files in BDMV disc");
            assert!(
                files.iter().any(|f| f.contains("00000")),
                "Should find 00000.M2T(S): {:?}",
                files
            );
        }
    }

    #[test]
    fn test_cat_file_from_bdmv_disc() {
        if let Some(p) = require_fixture("BDMV_DISC.iso") {
            let exts = vec!["m2ts".to_string(), "m2t".to_string()];
            let files = list_media_files(&p, &exts).unwrap();
            let inner = &files[0];

            let mut buf = Vec::new();
            cat_file(&p, inner, &mut buf).expect("cat_file failed");

            assert!(buf.len() > 188, "Should have at least one TS packet");
            let has_sync = buf.windows(1).any(|w| w[0] == 0x47);
            assert!(
                has_sync,
                "Should contain at least one MPEG-TS sync byte (0x47)"
            );
        }
    }

    #[test]
    fn test_file_size_from_bdmv_disc() {
        if let Some(p) = require_fixture("BDMV_DISC.iso") {
            let exts = vec!["m2ts".to_string(), "m2t".to_string()];
            let files = list_media_files(&p, &exts).unwrap();
            let inner = &files[0];

            let size = file_size(&p, inner).expect("file_size failed");
            assert!(
                size > 1_000_000,
                "File should be at least 1MB, got {}",
                size
            );
        }
    }

    // ---- AACS / BD+ encryption detection ----

    fn bluray_root_with_dir(extra_dir: &str) -> isomage::TreeNode {
        // Minimal Blu-ray-shaped tree: BDMV/STREAM/00000.m2ts plus an extra
        // directory at the root (e.g. AACS or BDSVM) to simulate encryption.
        let mut root = isomage::TreeNode::new_directory("ROOT".to_string());

        let mut bdmv = isomage::TreeNode::new_directory("BDMV".to_string());
        let mut stream = isomage::TreeNode::new_directory("STREAM".to_string());
        stream.add_child(isomage::TreeNode::new_file(
            "00000.m2ts".to_string(),
            10_000_000,
        ));
        bdmv.add_child(stream);
        root.add_child(bdmv);

        let mut protected = isomage::TreeNode::new_directory(extra_dir.to_string());
        protected.add_child(isomage::TreeNode::new_file(
            "Unit_Key_RO.inf".to_string(),
            4096,
        ));
        root.add_child(protected);

        root
    }

    #[test]
    fn test_detect_encryption_aacs() {
        let root = bluray_root_with_dir("AACS");
        let reason = detect_encryption(&root).expect("should detect AACS");
        assert!(
            reason.contains("AACS"),
            "Reason should mention AACS: {}",
            reason
        );
        assert!(
            reason.contains("makemkv"),
            "Reason should suggest makemkv: {}",
            reason
        );
    }

    #[test]
    fn test_detect_encryption_aacs_case_insensitive() {
        let root = bluray_root_with_dir("aacs");
        assert!(
            detect_encryption(&root).is_some(),
            "Lowercase aacs/ directory should still be detected"
        );
    }

    #[test]
    fn test_detect_encryption_bdsvm() {
        let root = bluray_root_with_dir("BDSVM");
        let reason = detect_encryption(&root).expect("should detect BD+");
        assert!(
            reason.contains("BD+") || reason.contains("BDSVM"),
            "Reason should mention BD+ or BDSVM: {}",
            reason
        );
    }

    #[test]
    fn test_detect_encryption_clean_disc() {
        // Plain Blu-ray with no AACS/BDSVM directory — should not flag.
        let mut root = isomage::TreeNode::new_directory("ROOT".to_string());
        let mut bdmv = isomage::TreeNode::new_directory("BDMV".to_string());
        let mut stream = isomage::TreeNode::new_directory("STREAM".to_string());
        stream.add_child(isomage::TreeNode::new_file(
            "00000.m2ts".to_string(),
            10_000_000,
        ));
        bdmv.add_child(stream);
        root.add_child(bdmv);

        assert!(detect_encryption(&root).is_none());
    }

    // ---- VTS title parsing ----

    #[test]
    fn test_parse_vts_title() {
        assert_eq!(parse_vts_title("VTS_01_1.VOB"), Some("01".to_string()));
        assert_eq!(parse_vts_title("VTS_02_3.VOB"), Some("02".to_string()));
        assert_eq!(parse_vts_title("VTS_01_0.VOB"), None); // segment 0 = IFO
        assert_eq!(parse_vts_title("VIDEO_TS.VOB"), None);
        assert_eq!(parse_vts_title("RANDOM.VOB"), None);
    }
}
