use std::{collections::HashSet, sync::Arc};

use chariot_rootfs::{CachedPkgSet, RootFS};

use crate::{
    buildcache::BuildCache,
    config::{Config, package::PackagePlatform},
    store::Store,
    workdir::WorkDirectoryParent,
};

pub mod buildcache;
pub mod config;
pub mod dependencies;
mod execenv;
pub mod package;
pub mod source;
pub mod store;
pub mod workdir;
pub mod xbps;

pub const HOST_ARCH: &str = "x86_64";
pub const NOARCH_ARCH: &str = "noarch";

pub const HOST_PREFIX: &str = "/usr/local";
pub const DEFAULT_TARGET_PREFIX: &str = "/usr";

pub struct CoreContext {
    pub build_cache_enabled: HashSet<(PackagePlatform, String)>,
    pub parallelism: usize,
    pub rootfs: Arc<RootFS>,
    pub store: Arc<Store>,
    pub build_cache: Arc<BuildCache>,
    pub workdir_parent: Arc<WorkDirectoryParent>,
    pub root_pkgset: Option<Arc<CachedPkgSet>>,
    pub git_pkgset: Option<Arc<CachedPkgSet>>,
    pub wget_pkgset: Option<Arc<CachedPkgSet>>,
    pub sha256sum_pkgset: Option<Arc<CachedPkgSet>>,
    pub bsdtar_pkgset: Option<Arc<CachedPkgSet>>,
    pub patch_pkgset: Option<Arc<CachedPkgSet>>,
}

pub fn collect_all_hashes(config: &Config) -> HashSet<(&'static str, u128)> {
    let mut hashes = HashSet::new();
    for pkg in &config.packages {
        hashes.insert(("install", pkg.get_content_hash()));
        hashes.insert(("pkg", pkg.get_package_hash()));
    }
    for src in &config.sources {
        let (base_hash, patch_hash, prepare_hash) = src.get_hashes();
        hashes.insert(("source.base", base_hash));
        hashes.insert(("source.patch", patch_hash));
        hashes.insert(("source.prepare", prepare_hash));
    }
    hashes
}
