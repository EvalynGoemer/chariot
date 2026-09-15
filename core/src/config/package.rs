use std::{
    collections::HashSet,
    hash::{Hash, Hasher},
    sync::Arc,
};

use xxhash_rust::xxh3::Xxh3;

use crate::config::{ConfigEnv, Dependencies, script::Script};

#[derive(Debug, Clone, Copy, PartialEq, Hash)]
pub enum PackagePlatform {
    Host,
    Target,
}

pub struct Package {
    pub config_env: Arc<ConfigEnv>,
    pub platform: PackagePlatform,
    pub name: String,
    pub version: String,
    pub revision: usize,
    pub dependencies: Dependencies,
    pub runtime_dependencies: Vec<Arc<Package>>,
    /// must contain keys into effective_options
    pub subscribed_options: HashSet<String>,
    pub configure: Option<Script>,
    pub build: Option<Script>,
    pub install: Script,
}

impl Package {
    pub fn get_content_hash(&self) -> u64 {
        let mut hasher = Xxh3::new();
        self.config_env.rootfs_manifest_hash.hash(&mut hasher);
        self.config_env.target_prefix.hash(&mut hasher);
        self.config_env.resolve_subscribed_options(&self.subscribed_options).hash(&mut hasher);
        self.dependencies.hash(&mut hasher);
        self.configure.hash(&mut hasher);
        self.build.hash(&mut hasher);
        self.install.hash(&mut hasher);
        hasher.finish()
    }

    pub fn get_package_hash(&self) -> u64 {
        let mut hasher = Xxh3::new();
        self.get_content_hash().hash(&mut hasher);
        self.platform.hash(&mut hasher);
        self.name.hash(&mut hasher);
        self.version.hash(&mut hasher);
        self.revision.hash(&mut hasher);
        self.runtime_dependencies.hash(&mut hasher);
        hasher.finish()
    }
}

impl Hash for Package {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.get_package_hash());
    }
}
