use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    hash::Hash,
    sync::Arc,
};

use crate::config::{package::Package, source::Source};

pub mod package;
pub mod script;
pub mod source;

#[derive(Hash)]
pub struct Dependencies {
    pub native: BTreeSet<String>,
    pub sources: BTreeMap<String, Arc<Source>>,
    pub tools: Vec<Arc<Package>>,
    pub packages: Vec<Arc<Package>>,
}

pub struct Config {
    pub env: Arc<ConfigEnv>,

    /// all packages referenced by config must be here
    pub packages: Vec<Arc<Package>>,

    /// all sources referenced by config must be here
    pub sources: Vec<Arc<Source>>,
}

pub struct ConfigEnv {
    pub rootfs_manifest_hash: String,

    pub target_prefix: String,

    /// key must be alphanumeric
    pub effective_options: HashMap<String, String>,
}

impl ConfigEnv {
    pub fn resolve_subscribed_options(&self, subscribed_options: &HashSet<String>) -> BTreeMap<String, String> {
        subscribed_options
            .iter()
            .map(|key| (key.clone(), self.effective_options[key].clone()))
            .collect()
    }
}
