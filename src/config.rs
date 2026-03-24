use anyhow::Result;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The default `config.yaml` baked in at compile time.
pub const EMBEDDED: &str = include_str!("../config.yaml");

#[derive(Debug, Deserialize)]
pub struct Config {
    pub target: TargetConfig,
    pub media_extensions: Vec<String>,
    #[serde(default)]
    pub output_dir: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct TargetConfig {
    pub codec: String,
    #[serde(default = "default_quality")]
    pub quality: u32,
    #[serde(default = "default_preset")]
    pub preset: String,
    #[serde(default = "default_max_width")]
    pub max_width: u32,
    #[serde(default = "default_max_height")]
    pub max_height: u32,
    #[serde(default)]
    pub max_bitrate_kbps: u32,
    #[serde(default = "default_container")]
    pub container: String,
    #[serde(default = "default_audio_codec")]
    pub audio_codec: String,
    #[serde(default = "default_subtitle_codec")]
    pub subtitle_codec: String,
}

fn default_quality() -> u32 {
    28
}
fn default_preset() -> String {
    "slow".to_string()
}
fn default_max_width() -> u32 {
    3840
}
fn default_max_height() -> u32 {
    2160
}
fn default_container() -> String {
    "mkv".to_string()
}
fn default_audio_codec() -> String {
    "copy".to_string()
}
fn default_subtitle_codec() -> String {
    "copy".to_string()
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = serde_yaml::from_str(&content)?;
        Ok(config)
    }

    /// Parse the built-in default config embedded at compile time.
    /// Panics if the embedded YAML is invalid (a compile-time invariant).
    pub fn from_embedded() -> Self {
        serde_yaml::from_str(EMBEDDED).expect("embedded config.yaml is invalid YAML")
    }
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.target.codec, "hevc");
        assert_eq!(config.target.quality, 22);
        assert_eq!(config.media_extensions.len(), 2);
    }

    #[test]
    fn test_embedded_const_is_valid_yaml() {
        // The EMBEDDED constant must always parse cleanly — it's the shipped defaults.
        let result: Result<Config, _> = serde_yaml::from_str(EMBEDDED);
        assert!(result.is_ok(), "EMBEDDED config.yaml failed to parse: {:?}", result.err());
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
        // Config::load on a nonexistent path must return Err (not panic).
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
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.target.quality, 28);
        assert_eq!(config.target.preset, "slow");
        assert_eq!(config.target.max_width, 3840);
        assert_eq!(config.target.container, "mkv");
    }
}
