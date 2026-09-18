use std::{
    collections::HashMap,
    fs::{exists, read_to_string},
    path::Path,
};

use anyhow::Result;
use serde::Deserialize;

#[derive(Default, Deserialize)]
#[serde(default)]
pub struct PackageConfig {
    pub enable_build_cache: bool,
}

#[derive(Default, Deserialize)]
#[serde(default)]
pub struct CliConfig {
    pub pkgs: HashMap<String, PackageConfig>,
    pub tools: HashMap<String, PackageConfig>,
}

pub fn parse_cli_config(path: impl AsRef<Path>) -> Result<CliConfig> {
    if !exists(&path)? {
        return Ok(CliConfig::default());
    }

    let data = read_to_string(&path)?;
    let config = toml::from_str::<CliConfig>(&data)?;
    Ok(config)
}
