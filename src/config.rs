use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The default `config.yaml` baked in at compile time.
pub const EMBEDDED: &str = include_str!("../config.yaml");

#[derive(Debug, Clone)]
pub struct Config {
    pub target: TargetConfig,
    pub media_extensions: Vec<String>,
    pub output_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TargetConfig {
    pub codec: String,
    pub quality: u32,
    pub preset: String,
    pub max_width: u32,
    pub max_height: u32,
    pub max_bitrate_kbps: u32,
    pub container: String,
    pub audio_codec: String,
    pub subtitle_codec: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        parse_config(&content).with_context(|| format!("parsing {}", path.display()))
    }

    /// Parse the built-in default config embedded at compile time.
    /// Panics if the embedded YAML is invalid (a compile-time invariant).
    pub fn from_embedded() -> Self {
        parse_config(EMBEDDED).expect("embedded config.yaml is invalid")
    }
}

/// Minimal hand-rolled parser for the hvac config.yaml subset:
/// top-level scalar keys, a `target:` mapping, and a `media_extensions:` sequence.
/// Handles blank lines and `#`-prefixed comment lines; unknown keys are ignored.
fn parse_config(yaml: &str) -> Result<Config> {
    #[derive(PartialEq)]
    enum Section {
        None,
        Target,
        MediaExtensions,
    }

    let mut section = Section::None;
    let mut codec: Option<String> = None;
    let mut quality: u32 = 28;
    let mut preset = String::from("slow");
    let mut max_width: u32 = 3840;
    let mut max_height: u32 = 2160;
    let mut max_bitrate_kbps: u32 = 0;
    let mut container = String::from("mkv");
    let mut audio_codec = String::from("copy");
    let mut subtitle_codec = String::from("copy");
    let mut media_extensions: Vec<String> = Vec::new();
    let mut output_dir: Option<PathBuf> = None;

    for (i, line) in yaml.lines().enumerate() {
        let lineno = i + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let indent = line.len() - line.trim_start().len();

        if indent == 0 {
            if trimmed == "target:" {
                section = Section::Target;
            } else if trimmed == "media_extensions:" {
                section = Section::MediaExtensions;
            } else if let Some(val) = trimmed.strip_prefix("output_dir:") {
                let val = val.trim();
                output_dir = if val.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(val))
                };
                section = Section::None;
            } else {
                section = Section::None;
            }
            continue;
        }

        match section {
            Section::Target => {
                if let Some((k, v)) = trimmed.split_once(':') {
                    let v = v.trim();
                    match k.trim() {
                        "codec" => codec = Some(v.to_string()),
                        "quality" => {
                            quality = v.parse().with_context(|| {
                                format!("line {lineno}: quality must be an integer")
                            })?
                        }
                        "preset" => preset = v.to_string(),
                        "max_width" => {
                            max_width = v.parse().with_context(|| {
                                format!("line {lineno}: max_width must be an integer")
                            })?
                        }
                        "max_height" => {
                            max_height = v.parse().with_context(|| {
                                format!("line {lineno}: max_height must be an integer")
                            })?
                        }
                        "max_bitrate_kbps" => {
                            max_bitrate_kbps = v.parse().with_context(|| {
                                format!("line {lineno}: max_bitrate_kbps must be an integer")
                            })?
                        }
                        "container" => container = v.to_string(),
                        "audio_codec" => audio_codec = v.to_string(),
                        "subtitle_codec" => subtitle_codec = v.to_string(),
                        _ => {}
                    }
                }
            }
            Section::MediaExtensions => {
                if let Some(ext) = trimmed.strip_prefix("- ") {
                    let ext = ext.trim();
                    if !ext.is_empty() {
                        media_extensions.push(ext.to_string());
                    }
                }
            }
            Section::None => {}
        }
    }

    let codec = codec
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing required field: target.codec"))?;

    Ok(Config {
        target: TargetConfig {
            codec,
            quality,
            preset,
            max_width,
            max_height,
            max_bitrate_kbps,
            container,
            audio_codec,
            subtitle_codec,
        },
        media_extensions,
        output_dir,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_config() {
        let yaml = r#"
target:
  codec: hevc
  quality: 22
  preset: slow
  max_width: 1920
  max_height: 1080
  max_bitrate_kbps: 0
  container: mkv
  audio_codec: copy
  subtitle_codec: copy
media_extensions:
  - mkv
  - mp4
"#;
        let config = parse_config(yaml).unwrap();
        assert_eq!(config.target.codec, "hevc");
        assert_eq!(config.target.quality, 22);
        assert_eq!(config.media_extensions.len(), 2);
    }

    #[test]
    fn test_embedded_const_is_valid_yaml() {
        let result = parse_config(EMBEDDED);
        assert!(
            result.is_ok(),
            "EMBEDDED config.yaml failed to parse: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_from_embedded_has_expected_values() {
        let cfg = Config::from_embedded();
        assert_eq!(cfg.target.codec, "hevc");
        assert_eq!(cfg.target.quality, 28);
        assert_eq!(cfg.target.preset, "slow");
        assert_eq!(cfg.target.container, "mkv");
        assert_eq!(cfg.target.audio_codec, "copy");
        assert!(cfg.media_extensions.contains(&"mkv".to_string()));
        assert!(cfg.media_extensions.contains(&"mp4".to_string()));
        assert!(cfg.media_extensions.contains(&"iso".to_string()));
    }

    #[test]
    fn test_load_falls_back_gracefully_on_missing_file() {
        let result = Config::load(std::path::Path::new("/nonexistent/config.yaml"));
        assert!(result.is_err());
    }

    #[test]
    fn test_defaults() {
        let yaml = r#"
target:
  codec: hevc
media_extensions:
  - mkv
"#;
        let config = parse_config(yaml).unwrap();
        assert_eq!(config.target.quality, 28);
        assert_eq!(config.target.preset, "slow");
        assert_eq!(config.target.max_width, 3840);
        assert_eq!(config.target.container, "mkv");
    }
}
