use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::Arc,
};

use chariot_util::{
    current_time,
    fs::{FileSystemError, dir_entries, force_rm, make_path},
    lock::{DirLock, LockExclusive, block_attempted},
};

pub struct WorkDirectoryParent {
    path: PathBuf,
}

impl WorkDirectoryParent {
    pub fn get(path: impl AsRef<Path>) -> Result<Self, FileSystemError> {
        make_path(&path)?;

        let wdp = Self {
            path: path.as_ref().to_path_buf(),
        };

        wdp.purge_work_dir()?;

        Ok(wdp)
    }

    fn child_path(&self, id: u128) -> PathBuf {
        self.path.join(format!("{:x}", id))
    }

    fn purge_work_dir(&self) -> Result<(), FileSystemError> {
        for entry in dir_entries(&self.path)? {
            let _lock = match DirLock::exclusive_noblock(entry.path()) {
                Err(FileSystemError::Open { source, .. }) if source.kind() == ErrorKind::NotFound => continue,
                result if block_attempted(&result) => continue,
                result => result,
            }?;

            force_rm(entry.path())?;
        }
        Ok(())
    }
}

pub struct WorkDirectory {
    lock: Option<DirLock<LockExclusive>>,
    parent: Arc<WorkDirectoryParent>,
    id: u128,
}

impl WorkDirectory {
    pub fn create(parent: &Arc<WorkDirectoryParent>) -> Result<Self, FileSystemError> {
        loop {
            let id = current_time().as_nanos();
            let path = parent.child_path(id);
            make_path(&path)?;

            let lock = match DirLock::exclusive_noblock(&path) {
                Err(FileSystemError::Open { source, .. }) if source.kind() == ErrorKind::NotFound => continue,
                result if block_attempted(&result) => continue,
                result => result,
            }?;

            return Ok(Self {
                lock: Some(lock),
                parent: parent.clone(),
                id,
            });
        }
    }

    pub fn path(&self) -> PathBuf {
        self.parent.child_path(self.id)
    }

    pub fn persist(mut self) -> DirLock<LockExclusive> {
        self.lock.take().unwrap()
    }
}

impl Drop for WorkDirectory {
    fn drop(&mut self) {
        if self.lock.is_some() {
            let _ = force_rm(self.path());
        }
    }
}
