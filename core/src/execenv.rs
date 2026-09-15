use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use chariot_rootfs::{CachedPkgSet, RootFSOverlay};
use chariot_runtime::{Mount, MountKind, Overlay, RuntimeError};

use crate::{
    CoreContext,
    cache::{StoreEntry, WorkDirectory},
};

pub struct ExecEnv<'a> {
    pub ctx: &'a CoreContext,
    pub pkgset: Option<Arc<CachedPkgSet>>,
    pub sources: HashMap<String, Vec<StoreEntry>>,
    pub sysroot: WorkDirectory,
    pub tool_overlay: Option<WorkDirectory>,
}

impl<'a> ExecEnv<'a> {
    pub fn exec(
        &self,
        cwd: impl AsRef<Path>,
        mounts: Vec<&Mount>,
        environment: &HashMap<impl AsRef<str>, impl AsRef<str>>,
        logger: &mut dyn Write,
        args: Vec<impl AsRef<str>>,
    ) -> Result<i32, RuntimeError> {
        let source_mounts = self
            .sources
            .iter()
            .map(|(name, store_entries)| Mount {
                dest: PathBuf::from("/chariot/sources").join(name),
                kind: MountKind::OverlayFS(Overlay {
                    upper_directory: None,
                    lower_directories: store_entries.iter().map(|entry| entry.path()).rev().collect(),
                }),
            })
            .collect::<Vec<_>>();

        let sysroot_mount = Mount {
            dest: PathBuf::from("/chariot/sysroot"),
            kind: MountKind::Bind {
                from: self.sysroot.path(),
                read_only: false,
                is_file: false,
            },
        };

        let parallelism_string = self.ctx.parallelism.to_string();

        let base_mounts = source_mounts.into_iter().chain([sysroot_mount]).collect::<Vec<_>>();
        let base_env = HashMap::from([
            ("SOURCES_DIR", "/chariot/sources"),
            ("SYSROOT_DIR", "/chariot/sysroot"),
            ("PARALLELISM", &parallelism_string),
        ]);

        self.ctx.rootfs.exec(
            cwd,
            &base_mounts.iter().chain(mounts).collect(),
            &base_env
                .into_iter()
                .chain(environment.iter().map(|(k, v)| (k.as_ref(), v.as_ref())))
                .collect(),
            logger,
            args,
            self.pkgset.as_deref(),
            self.tool_overlay.as_ref().map(|workdir| RootFSOverlay::ReadOnly(workdir.path())),
        )
    }
}
