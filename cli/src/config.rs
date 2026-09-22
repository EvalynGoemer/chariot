use std::{
    collections::HashMap,
    fs::{exists, read_to_string},
    path::{Path, PathBuf},
};

use anyhow::Result;
use chariot_core::config::package::PackagePlatform;
use serde::Deserialize;

#[derive(Default, Deserialize)]
#[serde(default)]
pub struct PackageConfig {
    pub enable_build_cache: bool,
    pub source_overrides: HashMap<String, PathBuf>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
pub struct CliConfig {
    pub pkgs: HashMap<String, PackageConfig>,
    pub tools: HashMap<String, PackageConfig>,
}

impl CliConfig {
    pub fn get_source_override_map(&self) -> HashMap<(String, PackagePlatform), HashMap<String, PathBuf>> {
        self.pkgs
            .iter()
            .map(|(name, config)| ((name.clone(), PackagePlatform::Target), config.source_overrides.clone()))
            .chain(
                self.tools
                    .iter()
                    .map(|(name, config)| ((name.clone(), PackagePlatform::Host), config.source_overrides.clone())),
            )
            .collect()
    }
}

pub fn parse_cli_config(path: impl AsRef<Path>) -> Result<CliConfig> {
    if !exists(&path)? {
        return Ok(CliConfig::default());
    }

    let data = read_to_string(&path)?;
    let config = toml::from_str::<CliConfig>(&data)?;
    Ok(config)
}
