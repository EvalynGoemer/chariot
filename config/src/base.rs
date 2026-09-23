use std::{
    collections::{BTreeMap, BTreeSet},
    fs::read_to_string,
    path::{Path, PathBuf},
};

use chariot_util::fs::FileSystemError;
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Failed to read base config")]
    ReadBaseConfig(#[source] FileSystemError),

    #[error("Failed to parse base config")]
    ParseBaseConfig(#[source] toml::de::Error),
}

#[derive(Deserialize)]
pub struct RootFSConfig {
    pub version: String,
    pub hash: String,
    pub url: Option<String>,
}

#[derive(Deserialize)]
pub struct BaseConfig {
    pub lua_root: Option<PathBuf>,
    pub target_prefix: Option<String>,
    #[serde(default)]
    pub global_native_packages: BTreeSet<String>,
    #[serde(default)]
    pub global_environment_variables: BTreeMap<String, String>,
    pub rootfs: RootFSConfig,
}

pub fn read_base_config(path: impl AsRef<Path>) -> Result<BaseConfig, ConfigError> {
    let base_config_text = read_to_string(&path).map_err(|err| {
        ConfigError::ReadBaseConfig(FileSystemError::ReadFile {
            path: path.as_ref().to_path_buf(),
            source: err,
        })
    })?;

    Ok(toml::from_str::<BaseConfig>(&base_config_text).map_err(|err| ConfigError::ParseBaseConfig(err))?)
}
