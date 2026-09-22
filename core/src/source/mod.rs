use std::{collections::HashMap, hash::Hash, io::Write, path::PathBuf};

use chariot_rootfs::{CachedPkgSet, GetPkgSetError};
use chariot_runtime::{Mount, MountKind::OverlayFS, Overlay, OverlayUpperDirectory, RuntimeError};
use chariot_util::fs::{FileSystemError, copy_recursive};
use thiserror::Error;
use xxhash_rust::xxh3::Xxh3;

use crate::{
    CoreContext,
    config::{
        script::Script,
        source::{Source, SourceBase},
    },
    execenv::{CreateExecEnvError, ExecEnv},
    source::{
        archive::{ArchiveFetchError, fetch_archive},
        git::{GitFetchError, fetch_git_repository},
    },
    store::StoreEntry,
    workdir::WorkDirectory,
};

mod archive;
mod git;

#[derive(Debug, Error)]
pub enum SourceFetchError {
    #[error(transparent)]
    FileSystem(#[from] FileSystemError),

    #[error(transparent)]
    Runtime(#[from] RuntimeError),

    #[error(transparent)]
    Database(#[from] rusqlite::Error),

    #[error(transparent)]
    ResolveDependencies(#[from] Box<CreateExecEnvError>), // TODO: this box is nasty

    #[error(transparent)]
    GetPkgSet(#[from] GetPkgSetError),

    #[error(transparent)]
    Archive(#[from] ArchiveFetchError),

    #[error(transparent)]
    Git(#[from] GitFetchError),

    #[error("Patch failed")]
    Patch,

    #[error("Prepare failed")]
    Prepare,
}

pub fn fetch_source(ctx: &CoreContext, logger: &mut dyn Write, source: &Source) -> Result<Vec<StoreEntry>, SourceFetchError> {
    let mut store_entries = Vec::new();

    let base_hash = source.get_base_hash();
    let base_store_entry = match StoreEntry::get(&ctx.store, "source.base", base_hash)? {
        Some(store_entry) => store_entry,
        None => StoreEntry::from_workdir(
            &ctx.store,
            match &source.base {
                SourceBase::Archive(archive) => fetch_archive(ctx, logger, &archive)?,
                SourceBase::Git(git_source) => fetch_git_repository(ctx, logger, &git_source)?,
                SourceBase::Local(local_source) => {
                    let work_dir = WorkDirectory::create(&ctx.workdir_parent)?;
                    copy_recursive(&local_source.path, work_dir.path())?;
                    work_dir
                }
            },
            "source.base",
            base_hash,
        )?,
    };
    store_entries.push(base_store_entry);

    let patch_hash = source.get_patch_hash(base_hash);
    if source.patches.len() > 0 {
        let patched_store_entry = match StoreEntry::get(&ctx.store, "source.patch", patch_hash)? {
            Some(store_entry) => store_entry,
            None => {
                let overlay_work_directory = WorkDirectory::create(&ctx.workdir_parent)?;
                let work_directory = WorkDirectory::create(&ctx.workdir_parent)?;

                for patch in &source.patches {
                    let exit_code = ctx.rootfs.exec(
                        "/chariot/source",
                        &vec![&Mount {
                            dest: PathBuf::from("/chariot/source"),
                            kind: OverlayFS(Overlay {
                                lower_directories: store_entries.iter().map(|entry| entry.path()).collect(),
                                upper_directory: Some(OverlayUpperDirectory {
                                    upper_directory: work_directory.path(),
                                    work_directory: overlay_work_directory.path(),
                                }),
                            }),
                        }],
                        &HashMap::from([("CHARIOT_PATCH", patch)]),
                        logger,
                        Script::bash("echo \"$CHARIOT_PATCH\" | patch -p1").command(),
                        ctx.patch_pkgset.as_deref(),
                        None,
                    )?;

                    if exit_code != 0 {
                        return Err(SourceFetchError::Patch);
                    }
                }

                StoreEntry::from_workdir(&ctx.store, work_directory, "source.patch", patch_hash)?
            }
        };
        store_entries.push(patched_store_entry);
    };

    if let Some(prepare) = &source.prepare {
        let prepare_hash_base = source.get_prepare_base_hash(patch_hash);
        let prepare_hash = source.get_prepare_hash(prepare_hash_base);

        let cached_entry = match ctx.ledger.lookup("source.prepare", prepare_hash)? {
            Some(effective_hash) => StoreEntry::get(&ctx.store, "source.prepare", effective_hash)?,
            None => None,
        };

        let prepare_store_entry = match cached_entry {
            Some(store_entry) => store_entry,
            None => {
                let pkgset = CachedPkgSet::get(&ctx.rootfs, &ctx.root_pkgset, &prepare.dependencies.native, logger)?;
                let exec_env = ExecEnv::create(
                    ctx,
                    logger,
                    pkgset,
                    &prepare.dependencies.sources,
                    &prepare.dependencies.packages,
                    &prepare.dependencies.tools,
                )
                .map_err(|err| Box::new(err))?;

                let effective_hash = {
                    let mut hasher = Xxh3::new();
                    prepare_hash_base.hash(&mut hasher);
                    exec_env.compute_deps_hash()?.hash(&mut hasher);
                    hasher.digest128()
                };

                let entry = match StoreEntry::get(&ctx.store, "source.prepare", effective_hash)? {
                    Some(store_entry) => store_entry,
                    None => {
                        let overlay_work_directory = WorkDirectory::create(&ctx.workdir_parent)?;
                        let work_directory = WorkDirectory::create(&ctx.workdir_parent)?;

                        let exit_code = exec_env.exec(
                            "/chariot/source",
                            vec![&Mount {
                                dest: PathBuf::from("/chariot/source"),
                                kind: OverlayFS(Overlay {
                                    lower_directories: store_entries.iter().map(|entry| entry.path()).collect(),
                                    upper_directory: Some(OverlayUpperDirectory {
                                        upper_directory: work_directory.path(),
                                        work_directory: overlay_work_directory.path(),
                                    }),
                                }),
                            }],
                            &prepare
                                .global_env
                                .global_environment_variables
                                .iter()
                                .chain(&prepare.environment_variables)
                                .map(|(k, v)| (k.as_str(), v.as_str()))
                                .chain([("SOURCE_DIR", "/chariot/source")])
                                .collect(),
                            logger,
                            prepare.script.command(),
                        )?;

                        if exit_code != 0 {
                            return Err(SourceFetchError::Prepare);
                        }

                        StoreEntry::from_workdir(&ctx.store, work_directory, "source.prepare", effective_hash)?
                    }
                };

                ctx.ledger.record("source.prepare", prepare_hash, effective_hash)?;
                entry
            }
        };
        store_entries.push(prepare_store_entry);
    }

    Ok(store_entries)
}
