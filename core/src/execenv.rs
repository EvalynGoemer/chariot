use std::{
    collections::{BTreeMap, HashMap},
    hash::{Hash, Hasher},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use chariot_rootfs::CachedPkgSet;
use chariot_runtime::{Mount, MountKind, Overlay, OverlayUpperDirectory, RuntimeError, StderrTarget};
use chariot_util::{fs::FileSystemError, hash::hash_directory};
use thiserror::Error;
use xxhash_rust::xxh3::Xxh3;

use crate::{
    CoreContext, HOST_ARCH,
    config::{
        package::{Package, PackagePlatform},
        source::Source,
    },
    package::{ProcessPackageError, resolve_package_runtime_dependencies},
    source::{SourceFetchError, fetch_source},
    store::StoreEntry,
    workdir::WorkDirectory,
    xbps::{XBPSPackageInstallError, package_install},
};

pub const EXECENV_SOURCES_DIRECTORY_PATH: &str = "/chariot/sources";
pub const EXECENV_SYSROOT_DIRECTORY_PATH: &str = "/chariot/sysroot";

#[derive(Debug, Error)]
pub enum CreateExecEnvError {
    #[error(transparent)]
    FileSystem(#[from] FileSystemError),

    #[error(transparent)]
    ProcessPackage(#[from] ProcessPackageError),

    #[error("Failed to fetch source `{}`", name)]
    FetchSource { name: String, source: SourceFetchError },

    #[error("Failed to install {} package `{}`", platform.to_string(), name)]
    PackageInstall {
        platform: PackagePlatform,
        name: String,
        source: XBPSPackageInstallError,
    },
}

pub struct ExecEnv<'a> {
    pub ctx: &'a CoreContext,
    pub pkgset: Option<Arc<CachedPkgSet>>,
    pub sources: HashMap<String, Vec<StoreEntry>>,
    pub sysroot: WorkDirectory,
    pub tool_overlay: Option<WorkDirectory>,
    pub root_readonly: bool,
    pub root_rw_overlay: Option<OverlayUpperDirectory>,
}

impl<'a> ExecEnv<'a> {
    pub fn create(
        ctx: &'a CoreContext,
        logger: &mut dyn Write,
        pkgset: Option<Arc<CachedPkgSet>>,
        sources: &BTreeMap<String, Arc<Source>>,
        packages: &Vec<Arc<Package>>,
        tools: &Vec<Arc<Package>>,
        root_readonly: bool,
        root_rw_overlay: Option<OverlayUpperDirectory>,
    ) -> Result<ExecEnv<'a>, CreateExecEnvError> {
        let mut cached_source_deps = HashMap::new();
        for (name, source) in sources {
            cached_source_deps.insert(
                name.clone(),
                fetch_source(ctx, logger, source).map_err(|err| CreateExecEnvError::FetchSource {
                    name: name.clone(),
                    source: err,
                })?,
            );
        }

        let sysroot = WorkDirectory::create(&ctx.workdir_parent)?;
        for pkg in packages {
            assert!(pkg.platform == PackagePlatform::Target);
            let entries = resolve_package_runtime_dependencies(ctx, logger, pkg)?;
            package_install(
                ctx,
                None,
                &pkg.name,
                &pkg.version,
                pkg.revision,
                &pkg.global_env.target_arch,
                entries.iter().map(|entry| entry.path()).collect(),
                &sysroot.path(),
                false,
                false,
                logger,
            )
            .map_err(|err| CreateExecEnvError::PackageInstall {
                platform: PackagePlatform::Target,
                name: pkg.name.clone(),
                source: err,
            })?;
        }

        let tool_overlay = if tools.len() == 0 {
            None
        } else {
            let tool_overlay_workdir = WorkDirectory::create(&ctx.workdir_parent)?;
            for tool in tools {
                assert!(tool.platform == PackagePlatform::Host);
                let entries = resolve_package_runtime_dependencies(ctx, logger, tool)?;
                package_install(
                    ctx,
                    pkgset.as_deref(),
                    &tool.name,
                    &tool.version,
                    tool.revision,
                    HOST_ARCH,
                    entries.iter().map(|entry| entry.path()).collect(),
                    &tool_overlay_workdir.path(),
                    true,
                    false,
                    logger,
                )
                .map_err(|err| CreateExecEnvError::PackageInstall {
                    platform: PackagePlatform::Host,
                    name: tool.name.clone(),
                    source: err,
                })?;
            }
            Some(tool_overlay_workdir)
        };

        Ok(Self {
            ctx,
            pkgset,
            sources: cached_source_deps,
            sysroot,
            tool_overlay,
            root_readonly,
            root_rw_overlay,
        })
    }

    pub fn compute_deps_hash(&self) -> Result<u128, FileSystemError> {
        let mut hasher = Xxh3::new();

        let mut names = self.sources.keys().collect::<Vec<_>>();
        names.sort();
        for name in names {
            name.hash(&mut hasher);
            for entry in &self.sources[name] {
                hash_directory(entry.path(), &mut hasher)?;
            }
        }

        hash_directory(self.sysroot.path(), &mut hasher)?;

        match &self.tool_overlay {
            Some(tool_overlay) => {
                hasher.write_u8(1);
                hash_directory(tool_overlay.path(), &mut hasher)?;
            }
            None => hasher.write_u8(0),
        }

        Ok(hasher.digest128())
    }

    pub fn exec(
        &self,
        cwd: impl AsRef<Path>,
        mounts: Vec<&Mount>,
        environment: &HashMap<impl AsRef<str>, impl AsRef<str>>,
        stdin: bool,
        stdout: Option<&mut dyn Write>,
        stderr: StderrTarget<'_>,
        args: Vec<impl AsRef<str>>,
    ) -> Result<i32, RuntimeError> {
        let source_mounts = self
            .sources
            .iter()
            .map(|(name, store_entries)| Mount {
                dest: PathBuf::from(EXECENV_SOURCES_DIRECTORY_PATH).join(name),
                kind: match store_entries.len() {
                    1 => MountKind::Bind {
                        from: store_entries[0].path(),
                        read_only: true,
                        is_file: false,
                    },
                    _ => MountKind::OverlayFS(Overlay {
                        upper_directory: None,
                        lower_directories: store_entries.iter().map(|entry| entry.path()).rev().collect(),
                    }),
                },
            })
            .collect::<Vec<_>>();

        let sysroot_mount = Mount {
            dest: PathBuf::from(EXECENV_SYSROOT_DIRECTORY_PATH),
            kind: MountKind::Bind {
                from: self.sysroot.path(),
                read_only: false,
                is_file: false,
            },
        };

        let mountpoint_mount = Mount {
            dest: PathBuf::from("/chariot"),
            kind: MountKind::FS {
                fstype: String::from("tmpfs"),
            },
        };

        let mountpoint_readonly_remount = Mount {
            dest: PathBuf::from("/chariot"),
            kind: MountKind::Remount { readonly: true },
        };

        let mut final_mounts = vec![&mountpoint_mount];
        for source_mount in &source_mounts {
            final_mounts.push(source_mount);
        }
        final_mounts.push(&sysroot_mount);
        for mount in mounts {
            final_mounts.push(mount);
        }
        final_mounts.push(&mountpoint_readonly_remount);

        let parallelism_string = self.ctx.parallelism.to_string();

        let base_env = HashMap::from([
            ("SOURCES_DIR", EXECENV_SOURCES_DIRECTORY_PATH),
            ("SYSROOT_DIR", EXECENV_SYSROOT_DIRECTORY_PATH),
            ("PARALLELISM", &parallelism_string),
        ]);

        self.ctx.rootfs.exec(
            cwd,
            &final_mounts,
            &base_env
                .into_iter()
                .chain(environment.iter().map(|(k, v)| (k.as_ref(), v.as_ref())))
                .collect(),
            stdin,
            stdout,
            stderr,
            args,
            self.pkgset.as_deref(),
            !self.root_readonly,
            self.root_rw_overlay.clone(),
            match &self.tool_overlay {
                None => Vec::new(),
                Some(workdir) => vec![workdir.path()],
            },
        )
    }
}
