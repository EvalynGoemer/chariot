use std::{
    collections::HashSet,
    fs::rename,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::Arc,
};

use chariot_util::{
    fs::{FileSystemError, dir_entries, force_rm, make_path},
    lock::{DirLock, LockShared, block_attempted},
};

use crate::workdir::WorkDirectory;

pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn get(path: impl AsRef<Path>) -> Result<Self, FileSystemError> {
        make_path(&path)?;

        Ok(Self {
            path: path.as_ref().to_path_buf(),
        })
    }

    fn entry_path(&self, category: &str, hash: u128) -> PathBuf {
        self.path.join(format!("{}-{:x}", category, hash))
    }

    pub fn prune_store(&self, exclude: HashSet<(&str, u64)>) -> Result<(), FileSystemError> {
        let _store_lock = DirLock::exclusive(&self.path)?;

        for entry in dir_entries(&self.path)? {
            if exclude
                .iter()
                .any(|(cat, hash)| entry.file_name().eq(format!("{}-{:x}", cat, hash).as_str()))
            {
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

pub struct StoreEntry {
    _lock: DirLock<LockShared>,
    store: Arc<Store>,
    category: String,
    hash: u128,
}

impl StoreEntry {
    pub fn get(store: &Arc<Store>, category: &str, hash: u128) -> Result<Option<Self>, FileSystemError> {
        let _store_lock = DirLock::shared(&store.path)?;

        let lock = match DirLock::shared_noblock(store.entry_path(category, hash)) {
            Ok(lock) => lock,
            Err(FileSystemError::Open { source, .. }) if source.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };

        Ok(Some(Self {
            _lock: lock,
            store: store.clone(),
            category: category.to_string(),
            hash,
        }))
    }

    pub fn from_workdir(store: &Arc<Store>, workdir: WorkDirectory, category: &str, hash: u128) -> Result<Self, FileSystemError> {
        let _store_lock = DirLock::shared(&store.path)?;

        let store_entry_path = store.entry_path(category, hash);
        match rename(workdir.path(), &store_entry_path) {
            Err(err) => {
                if err.kind() == ErrorKind::DirectoryNotEmpty {
                    if let Some(entry) = StoreEntry::get(store, category, hash)? {
                        return Ok(entry);
                    }
                }

                return Err(FileSystemError::Rename {
                    from: workdir.path(),
                    to: store_entry_path,
                    source: err,
                });
            }
            Ok(()) => {
                let lock = workdir.persist();
                return Ok(StoreEntry {
                    _lock: lock.relock_shared_noblock()?,
                    store: store.clone(),
                    category: category.to_string(),
                    hash,
                });
            }
        }
    }

    pub fn path(&self) -> PathBuf {
        self.store.entry_path(&self.category, self.hash)
    }
}
