use std::{
    collections::BTreeMap,
    hash::{Hash, Hasher},
    sync::Arc,
};

use xxhash_rust::xxh3::Xxh3;

use crate::config::{CONFIG_VERSION, Dependencies, GlobalEnvironment, script::Script};

#[derive(Debug, Hash)]
pub enum SourceBase {
    Archive(Archive),
    Git(GitSource),
}

#[derive(Debug, Hash)]
pub struct Archive {
    pub url: String,
    pub checksum: String,
    pub kind: ArchiveKind,
    pub compression: ArchiveCompression,
}

#[derive(Debug, Hash)]
pub enum ArchiveKind {
    Tar,
}

#[derive(Debug, Hash)]
pub enum ArchiveCompression {
    Xz,
    Gzip,
    Bzip2,
}

#[derive(Debug, Hash)]
pub struct GitSource {
    pub url: String,
    pub revision: String,
}

#[derive(Debug)]
pub struct SourcePrepare {
    pub global_env: Arc<GlobalEnvironment>,
    pub dependencies: Dependencies,
    pub environment_variables: BTreeMap<String, String>,
    pub script: Script,
}

#[derive(Debug)]
pub struct Source {
    pub base: SourceBase,
    pub patches: Vec<String>,
    pub prepare: Option<SourcePrepare>,
}

impl Source {
    pub fn get_hashes(&self) -> (u128, u128, u128) {
        let base_hash = {
            let mut hasher = Xxh3::new();
            CONFIG_VERSION.hash(&mut hasher);
            self.base.hash(&mut hasher);
            hasher.digest128()
        };

        let patch_hash = {
            let mut hasher = Xxh3::new();
            base_hash.hash(&mut hasher);
            self.patches.hash(&mut hasher);
            hasher.digest128()
        };

        let prepare_hash = {
            let mut hasher = Xxh3::new();
            patch_hash.hash(&mut hasher);
            if let Some(prepare) = &self.prepare {
                prepare.global_env.rootfs_manifest_hash.hash(&mut hasher);
                prepare.global_env.global_environment_variables.hash(&mut hasher);
                prepare.environment_variables.hash(&mut hasher);
                prepare.dependencies.hash(&mut hasher);
                prepare.script.hash(&mut hasher);
            }
            hasher.digest128()
        };

        (base_hash, patch_hash, prepare_hash)
    }
}

impl Hash for Source {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let (_, _, prepare_hash) = self.get_hashes();
        state.write_u128(prepare_hash);
    }
}
