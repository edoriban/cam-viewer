use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = "cameras.toml";

/// Corner of grid tiles where the status badge is drawn.
///
/// Declared scalar-first in [`Config`] so TOML serialization keeps plain keys
/// ahead of the `[[cameras]]` array-of-tables.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BadgePosition {
    TopRight,
    #[default]
    BottomRight,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraConfig {
    pub name: String,
    pub url: String,
}

/// Opt-out default for [`Config::update_check`]: a user who never heard of
/// this build has no other way to learn a fix shipped.
fn default_update_check() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub badge_position: BadgePosition,
    /// Whether to ask GitHub once per start whether a newer release exists.
    /// Absent from an existing file means enabled; setting it to `false` stops
    /// the app making any network request of its own.
    #[serde(default = "default_update_check")]
    pub update_check: bool,
    pub cameras: Vec<CameraConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            badge_position: BadgePosition::default(),
            update_check: default_update_check(),
            cameras: Vec::new(),
        }
    }
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
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
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
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let cfg: Config =
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        Config {
            badge_position: BadgePosition::BottomRight,
            update_check: true,
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

    #[test]
    fn legacy_toml_without_badge_position_defaults_to_bottom_right() {
        let raw = "[[cameras]]\nname = \"Front\"\nurl = \"rtsp://192.168.1.10/live\"\n";
        let parsed: Config = toml::from_str(raw).expect("parse legacy config");
        assert_eq!(parsed.badge_position, BadgePosition::BottomRight);
        assert_eq!(parsed.cameras.len(), 1);

        // The same legacy file must also survive a load through the public API.
        let root = std::env::temp_dir().join(format!("cam-viewer-legacy-{}", std::process::id()));
        let path = root.join(FILE_NAME);
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create dir");
        fs::write(&path, raw).expect("write legacy config");

        let loaded = load(&path).expect("load legacy config");
        assert_eq!(loaded.badge_position, BadgePosition::BottomRight);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn legacy_toml_without_update_check_defaults_to_enabled() {
        // Files written by 0.4.0 and earlier have no such key; they must keep
        // working and must not silently opt out of hearing about fixes.
        let raw = "[[cameras]]\nname = \"Front\"\nurl = \"rtsp://192.168.1.10/live\"\n";
        let parsed: Config = toml::from_str(raw).expect("parse legacy config");
        assert!(parsed.update_check, "absent key means enabled");
    }

    #[test]
    fn update_check_false_survives_a_round_trip() {
        // Opting out must actually stick, or the setting is a lie.
        let cfg = Config {
            update_check: false,
            ..sample()
        };
        let raw = serialize(&cfg).expect("serialize");
        let parsed: Config = toml::from_str(&raw).expect("parse");
        assert!(!parsed.update_check);
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn saved_toml_emits_badge_position_before_cameras_table_and_round_trips() {
        for position in [BadgePosition::TopRight, BadgePosition::BottomRight] {
            let cfg = Config {
                badge_position: position,
                update_check: true,
                cameras: sample().cameras,
            };
            let raw = serialize(&cfg).expect("serialize");

            let badge_idx = raw
                .find("badge_position")
                .expect("badge_position key emitted");
            let cameras_idx = raw.find("[[cameras]]").expect("cameras table emitted");
            assert!(
                badge_idx < cameras_idx,
                "scalar key must precede the array-of-tables header:\n{raw}"
            );
            assert!(
                raw.contains("badge_position = \"top-right\"")
                    == (position == BadgePosition::TopRight)
            );

            let parsed: Config = toml::from_str(&raw).expect("re-parse saved config");
            assert_eq!(parsed, cfg);
        }
    }
}
