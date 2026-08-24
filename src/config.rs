use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = "cameras.toml";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraConfig {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub cameras: Vec<CameraConfig>,
}

pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
    {
        return Some(dir.join(env!("CARGO_PKG_NAME")));
    }
    std::env::home_dir().map(|h| h.join(".config").join(env!("CARGO_PKG_NAME")))
}

pub fn config_path() -> PathBuf {
    match config_dir() {
        Some(dir) => dir.join(FILE_NAME),
        None => PathBuf::from(FILE_NAME),
    }
}

pub fn serialize(config: &Config) -> Result<String> {
    toml::to_string_pretty(config).context("serializing config")
}

pub fn save(path: &Path, config: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let serialized = serialize(config)?;
    fs::write(path, serialized).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn load(path: &Path) -> Result<Config> {
    if !path.exists() {
        let default = Config::default();
        save(path, &default)?;
        return Ok(default);
    }
    let raw =
        fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let cfg: Config =
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        Config {
            cameras: vec![
                CameraConfig {
                    name: "Front".to_owned(),
                    url: "rtsp://192.168.1.10/live".to_owned(),
                },
                CameraConfig {
                    name: "Back".to_owned(),
                    url: "rtsp://192.168.1.11/live".to_owned(),
                },
            ],
        }
    }

    #[test]
    fn serializes_and_parses_back_identically() {
        let cfg = sample();
        let raw = serialize(&cfg).expect("serialize");
        let parsed: Config = toml::from_str(&raw).expect("parse");
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn save_creates_missing_parent_dirs_and_load_round_trips() {
        let root = std::env::temp_dir().join(format!("cam-viewer-test-{}", std::process::id()));
        let path = root.join("nested").join(FILE_NAME);
        let _ = fs::remove_dir_all(&root);
        let cfg = sample();

        save(&path, &cfg).expect("save");
        let loaded = load(&path).expect("load");
        assert_eq!(loaded, cfg);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn load_returns_empty_camera_list_as_is() {
        let root = std::env::temp_dir().join(format!("cam-viewer-empty-{}", std::process::id()));
        let path = root.join(FILE_NAME);
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create dir");
        fs::write(&path, "cameras = []\n").expect("write empty config");

        let loaded = load(&path).expect("load");
        assert_eq!(loaded, Config::default());
        assert!(loaded.cameras.is_empty());

        let _ = fs::remove_dir_all(&root);
    }
}
