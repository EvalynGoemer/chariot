use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use chariot_util::{
    fs::{FileSystemError, dir_entries, force_rm, make_path},
    lock::{DirLock, LockExclusive, block_attempted},
};

use crate::config::package::PackagePlatform;

pub struct BuildCache {
    path: PathBuf,
}

impl BuildCache {
    pub fn get(path: impl AsRef<Path>) -> Result<Self, FileSystemError> {
        make_path(&path)?;

        Ok(Self {
            path: path.as_ref().to_path_buf(),
        })
    }

    fn dir_path(&self, platform: PackagePlatform, arch: &str, name: &str) -> PathBuf {
        self.path.join(format!("{}.{}.{}", platform.to_string(), name, arch))
    }

    pub fn prune(&self, exclude: HashSet<(PackagePlatform, String)>) -> Result<(), FileSystemError> {
        let _build_cache_lock = DirLock::exclusive(&self.path);

        for entry in dir_entries(&self.path)? {
            if exclude.iter().any(|(platform, name)| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(format!("{}.{}.", platform.to_string(), name).as_str())
            }) {
                continue;
            }

            let _lock = match DirLock::exclusive_noblock(entry.path()) {
                result if block_attempted(&result) => continue,
                result => result,
            }?;

            force_rm(entry.path())?;
        }

        Ok(())
    }
}

pub struct BuildDirectory {
    _lock: DirLock<LockExclusive>,
    build_cache: Arc<BuildCache>,
    platform: PackagePlatform,
    arch: String,
    name: String,
}

impl BuildDirectory {
    pub fn get(build_cache: &Arc<BuildCache>, platform: PackagePlatform, arch: &str, name: &str) -> Result<Self, FileSystemError> {
        let _build_cache_lock = DirLock::shared(&build_cache.path);

        let path = build_cache.dir_path(platform, &arch, &name);
        make_path(&path)?;

        let lock = DirLock::exclusive(path)?;

        Ok(BuildDirectory {
            _lock: lock,
            build_cache: build_cache.clone(),
            platform,
            arch: arch.to_string(),
            name: name.to_string(),
        })
    }

    pub fn path(&self) -> PathBuf {
        self.build_cache.dir_path(self.platform, &self.arch, &self.name)
    }
}
