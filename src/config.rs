use anyhow::Result;
use serde::Deserialize;
use std::path::{Path, PathBuf};

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
    22
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
    fn test_defaults() {
        let yaml = r#"
target:
  codec: hevc
media_extensions:
  - mkv
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.target.quality, 22);
        assert_eq!(config.target.preset, "slow");
        assert_eq!(config.target.max_width, 3840);
        assert_eq!(config.target.container, "mkv");
    }
}
