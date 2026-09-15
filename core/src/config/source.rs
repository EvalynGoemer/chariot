use std::{
    collections::BTreeMap,
    hash::{Hash, Hasher},
    sync::Arc,
};

use xxhash_rust::xxh3::Xxh3;

use crate::config::{CONFIG_VERSION, Dependencies, GlobalEnvironment, script::Script};

#[derive(Hash)]
pub enum SourceBase {
    Archive(Archive),
    Git(GitSource),
}

#[derive(Hash)]
pub struct Archive {
    pub url: String,
    pub checksum: String,
    pub kind: ArchiveKind,
    pub compression: ArchiveCompression,
}

#[derive(Hash)]
pub enum ArchiveKind {
    Tar,
}

#[derive(Hash)]
pub enum ArchiveCompression {
    Xz,
    Gzip,
    Bzip2,
}

#[derive(Hash)]
pub struct GitSource {
    pub url: String,
    pub revision: String,
}

#[derive(Hash)]
pub struct SourcePrepare {
    pub global_env: Arc<GlobalEnvironment>,
    pub dependencies: Dependencies,
    pub environment_variables: BTreeMap<String, String>,
    pub script: Script,
}

pub struct Source {
    pub base: SourceBase,
    pub patches: Vec<String>,
    pub prepare: Option<SourcePrepare>,
}

impl Source {
    pub fn get_hashes(&self) -> (u64, u64, u64) {
        let base_hash = {
            let mut hasher = Xxh3::new();
            CONFIG_VERSION.hash(&mut hasher);
            self.base.hash(&mut hasher);
            hasher.finish()
        };

        let patch_hash = {
            let mut hasher = Xxh3::new();
            base_hash.hash(&mut hasher);
            self.patches.hash(&mut hasher);
            hasher.finish()
        };

        let prepare_hash = {
            let mut hasher = Xxh3::new();
            patch_hash.hash(&mut hasher);
            self.prepare.hash(&mut hasher);
            hasher.finish()
        };

        (base_hash, patch_hash, prepare_hash)
    }
}

impl Hash for Source {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let (_, _, prepare_hash) = self.get_hashes();
        state.write_u64(prepare_hash);
    }
}
