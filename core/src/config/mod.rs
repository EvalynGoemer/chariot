use std::{
    collections::{BTreeMap, BTreeSet},
    hash::Hash,
    sync::Arc,
};

use crate::config::{package::Package, source::Source};

pub mod package;
pub mod script;
pub mod source;

const CONFIG_VERSION: u64 = 1;

#[derive(Debug, Default, Hash)]
pub struct Dependencies {
    pub native: BTreeSet<String>,
    pub sources: BTreeMap<String, Arc<Source>>,
    pub tools: Vec<Arc<Package>>,
    pub packages: Vec<Arc<Package>>,
}

#[derive(Debug)]
pub struct GlobalEnvironment {
    pub rootfs_manifest_hash: String,
    pub global_environment_variables: BTreeMap<String, String>,
    pub target_arch: String,
    pub target_prefix: String,
}

pub struct Config {
    pub global_env: Arc<GlobalEnvironment>,

    /// all packages referenced by config must be here
    pub packages: Vec<Arc<Package>>,

    /// all sources referenced by config must be here
    pub sources: Vec<Arc<Source>>,
}
