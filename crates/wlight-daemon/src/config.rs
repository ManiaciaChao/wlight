use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use wlight_core::DEFAULT_HARDWARE_FLOOR;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub hardware_floor: f64,
    pub displays: BTreeMap<String, SavedDisplay>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hardware_floor: DEFAULT_HARDWARE_FLOOR,
            displays: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedDisplay {
    pub ddc_brightness: Option<u16>,
    pub gamma_brightness: Option<f64>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let source = match fs::read_to_string(path) {
            Ok(source) => source,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        let config: Self = toml::from_str(&source)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if !self.hardware_floor.is_finite() || !(0.0..=1.0).contains(&self.hardware_floor) {
            bail!("hardware_floor must be finite and between 0.0 and 1.0");
        }
        for (id, display) in &self.displays {
            if display.ddc_brightness.is_some_and(|value| value > 100) {
                bail!("display {id} has an invalid DDC brightness");
            }
            if display
                .gamma_brightness
                .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
            {
                bail!("display {id} has an invalid gamma brightness");
            }
        }
        Ok(())
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let parent = path
            .parent()
            .context("configuration path has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        let encoded = toml::to_string_pretty(self).context("failed to encode configuration")?;
        let temporary = path.with_extension("toml.tmp");
        fs::write(&temporary, encoded)
            .with_context(|| format!("failed to write {}", temporary.display()))?;
        fs::rename(&temporary, path).with_context(|| {
            format!(
                "failed to atomically replace {} with {}",
                path.display(),
                temporary.display()
            )
        })?;
        Ok(())
    }
}

pub fn default_path() -> Result<PathBuf> {
    let directories = ProjectDirs::from("io.github", "wlight", "wlight")
        .context("could not determine the user configuration directory")?;
    Ok(directories.config_dir().join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_configuration() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("config.toml");
        let mut config = Config {
            hardware_floor: 0.25,
            ..Config::default()
        };
        config.displays.insert(
            "display-id".to_owned(),
            SavedDisplay {
                ddc_brightness: Some(42),
                gamma_brightness: Some(0.75),
            },
        );

        config.save(&path).expect("save configuration");
        let loaded = Config::load(&path).expect("load configuration");
        assert_eq!(loaded.hardware_floor, 0.25);
        let display = loaded.displays.get("display-id").expect("saved display");
        assert_eq!(display.ddc_brightness, Some(42));
        assert_eq!(display.gamma_brightness, Some(0.75));
    }

    #[test]
    fn rejects_out_of_range_values() {
        let config = Config {
            hardware_floor: 1.5,
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }
}
