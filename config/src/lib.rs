use std::{
    collections::BTreeMap,
    fs::read_to_string,
    path::{Path, PathBuf},
};

use chariot_core::{
    DEFAULT_TARGET_PREFIX,
    config::{Config, GlobalEnvironment},
};
use chariot_util::fs::FileSystemError;
use serde::Deserialize;
use thiserror::Error;

use crate::eval::eval_lua_config;

mod eval;

const DEFAULT_MAIN_CONFIG_PATH: &str = "./chariot.lua";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Failed to read base config")]
    ReadBaseConfig(#[source] FileSystemError),

    #[error("Failed to parse base config")]
    ParseBaseConfig(#[source] toml::de::Error),

    #[error("Lua evaluation error")]
    LuaEvalError(#[from] mlua::Error),
}

#[derive(Deserialize)]
pub struct RootFSConfig {
    pub version: String,
    pub hash: String,
    pub url: Option<String>,
}

#[derive(Deserialize)]
struct BaseConfig {
    lua_root: Option<PathBuf>,
    target_prefix: Option<String>,
    rootfs: RootFSConfig,
}

pub fn eval_config(base_config_path: impl AsRef<Path>, target_arch: String) -> Result<(Config, RootFSConfig), ConfigError> {
    let base_config_text = read_to_string(&base_config_path).map_err(|err| {
        ConfigError::ReadBaseConfig(FileSystemError::ReadFile {
            path: base_config_path.as_ref().to_path_buf(),
            source: err,
        })
    })?;
    let base_config = toml::from_str::<BaseConfig>(&base_config_text).map_err(|err| ConfigError::ParseBaseConfig(err))?;

    let global_environment = GlobalEnvironment {
        global_environment_variables: BTreeMap::new(),
        rootfs_manifest_hash: base_config.rootfs.hash.clone(),
        target_arch,
        target_prefix: base_config.target_prefix.unwrap_or(String::from(DEFAULT_TARGET_PREFIX)),
    };

    let lua_path = base_config.lua_root.unwrap_or(PathBuf::from(DEFAULT_MAIN_CONFIG_PATH));
    let config = eval_lua_config(&lua_path, global_environment)?;

    Ok((config, base_config.rootfs))
}
