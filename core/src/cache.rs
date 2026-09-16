use std::{
    collections::HashSet,
    fs::rename,
    io::ErrorKind,
    mem::ManuallyDrop,
    path::{Path, PathBuf},
    ptr,
    sync::Arc,
};

use chariot_util::{
    current_time,
    fs::{
        FileSystemError::{self},
        dir_entries, force_rm, make_path,
    },
    lock::{DirLock, LockExclusive, LockShared, block_attempted},
};

const SUBDIR_WORK: &str = "work";
const SUBDIR_STORE: &str = "store";

pub struct Cache {
    _lock: DirLock<LockExclusive>,
    path: PathBuf,
}

impl Cache {
    pub fn get(path: impl AsRef<Path>) -> Result<Self, FileSystemError> {
        make_path(&path)?;

        let lock = DirLock::exclusive(&path)?;

        let cache = Self {
            _lock: lock,
            path: path.as_ref().to_path_buf(),
        };

        for dir in [SUBDIR_WORK, SUBDIR_STORE] {
            make_path(cache.path.join(dir))?;
        }

        cache.purge_work_dir()?;

        Ok(cache)
    }

    fn path_work_directory(&self, id: u128) -> PathBuf {
        self.path.join(SUBDIR_WORK).join(format!("{:x}", id))
    }

    fn path_store_entry(&self, category: &str, hash: u64) -> PathBuf {
        self.path.join(SUBDIR_STORE).join(format!("{}-{:x}", category, hash))
    }

    fn purge_work_dir(&self) -> Result<(), FileSystemError> {
        let _workdir_lock = DirLock::exclusive(self.path.join(SUBDIR_WORK))?;

        for entry in dir_entries(self.path.join(SUBDIR_WORK))? {
            let _lock = match DirLock::exclusive_noblock(entry.path()) {
                result if block_attempted(&result) => continue,
                result => result,
            }?;

            force_rm(entry.path())?;
        }
        Ok(())
    }

    pub fn prune_store(&self, exclude: HashSet<(&str, u64)>) -> Result<(), FileSystemError> {
        let _store_lock = DirLock::exclusive(self.path.join(SUBDIR_STORE))?;

        for entry in dir_entries(self.path.join(SUBDIR_STORE))? {
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

pub struct WorkDirectory {
    lock: DirLock<LockExclusive>,
    cache: Arc<Cache>,
    id: u128,
}

impl WorkDirectory {
    pub fn create(cache: &Arc<Cache>) -> Result<Self, FileSystemError> {
        loop {
            let id = current_time().as_nanos();
            let path = cache.path_work_directory(id);
            make_path(&path)?;

            let lock = match DirLock::exclusive_noblock(&path) {
                result if block_attempted(&result) => continue,
                result => result,
            }?;

            return Ok(Self {
                lock,
                cache: cache.clone(),
                id,
            });
        }
    }

    pub fn path(&self) -> PathBuf {
        self.cache.path_work_directory(self.id)
    }

    pub fn move_to_store(self, category: &str, hash: u64) -> Result<StoreEntry, FileSystemError> {
        let _workdir_lock = DirLock::shared(self.cache.path.join(SUBDIR_WORK))?;
        let _store_lock = DirLock::shared(self.cache.path.join(SUBDIR_STORE))?;

        let store_entry_path = self.cache.path_store_entry(category, hash);
        match rename(self.path(), &store_entry_path) {
            Err(err) => {
                if err.kind() == ErrorKind::DirectoryNotEmpty {
                    if let Some(entry) = StoreEntry::get(&self.cache, category, hash)? {
                        return Ok(entry);
                    }
                }

                return Err(FileSystemError::Rename {
                    from: self.path(),
                    to: store_entry_path,
                    source: err,
                });
            }
            Ok(()) => {
                let this = ManuallyDrop::new(self);
                let (lock, cache) = unsafe { (ptr::read(&this.lock), ptr::read(&this.cache)) };
                return Ok(StoreEntry {
                    _lock: lock.relock_shared_noblock()?,
                    cache,
                    category: category.to_string(),
                    hash,
                });
            }
        }
    }
}

impl Drop for WorkDirectory {
    fn drop(&mut self) {
        let _ = force_rm(self.path());
    }
}

pub struct StoreEntry {
    _lock: DirLock<LockShared>,
    cache: Arc<Cache>,
    category: String,
    hash: u64,
}

impl StoreEntry {
    pub fn get(cache: &Arc<Cache>, category: &str, hash: u64) -> Result<Option<StoreEntry>, FileSystemError> {
        let lock = match DirLock::shared_noblock(cache.path_store_entry(category, hash)) {
            Ok(lock) => lock,
            Err(FileSystemError::Open { source, .. }) if source.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };

        Ok(Some(StoreEntry {
            _lock: lock,
            cache: cache.clone(),
            category: category.to_string(),
            hash,
        }))
    }

    pub fn path(&self) -> PathBuf {
        self.cache.path_store_entry(&self.category, self.hash)
    }

    pub fn hash(&self) -> u64 {
        self.hash
    }
}
