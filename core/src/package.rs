use std::{hash::Hash, io::Write, path::PathBuf};

use chariot_rootfs::{CachedPkgSet, GetPkgSetError};
use chariot_runtime::{Mount, MountKind, RuntimeError, StderrTarget};
use chariot_util::{fs::FileSystemError, hash::hash_directory};
use thiserror::Error;
use xxhash_rust::xxh3::Xxh3;

use crate::{
    CoreContext,
    buildcache::BuildDirectory,
    config::package::Package,
    execenv::{CreateExecEnvError, EXECENV_SOURCES_DIRECTORY_PATH, ExecEnv},
    source::SourceFetchError,
    store::StoreEntry,
    workdir::WorkDirectory,
    xbps::{XBPSPackageCreateError, package_create},
};

#[derive(Debug, Error)]
pub enum ProcessPackageError {
    #[error(transparent)]
    ResolveDependencies(#[from] Box<CreateExecEnvError>), // TODO: this box is nasty

    #[error(transparent)]
    FileSystem(#[from] FileSystemError),

    #[error(transparent)]
    Database(#[from] rusqlite::Error),

    #[error(transparent)]
    Runtime(#[from] RuntimeError),

    #[error(transparent)]
    GetPkgSet(#[from] GetPkgSetError),

    #[error(transparent)]
    PackageCreate(#[from] XBPSPackageCreateError),

    #[error("Failed to fetch source `{}`", name)]
    SourceFetch { name: String, source: SourceFetchError },

    #[error("Configure failed with the exit code {}", .0)]
    Configure(i32),

    #[error("Build failed with the exit code {}", .0)]
    Build(i32),

    #[error("Install failed with the exit code {}", .0)]
    Install(i32),
}

pub fn resolve_package_runtime_dependencies(
    ctx: &CoreContext,
    logger: &mut dyn Write,
    pkg: &Package,
) -> Result<Vec<StoreEntry>, ProcessPackageError> {
    let mut entries = vec![process_package(ctx, logger, pkg)?];

    for rdep in &pkg.runtime_dependencies {
        assert!(rdep.platform == pkg.platform);
        entries.extend(resolve_package_runtime_dependencies(ctx, logger, rdep)?);
    }

    Ok(entries)
}

pub fn process_package(ctx: &CoreContext, logger: &mut dyn Write, package: &Package) -> Result<StoreEntry, ProcessPackageError> {
    let pkg_hash = package.get_package_hash();

    if let Some(effective_hash) = ctx.ledger.lookup("pkg", pkg_hash)?
        && let Some(store_entry) = StoreEntry::get(&ctx.store, "pkg", effective_hash)?
    {
        return Ok(store_entry);
    }

    let install_store_entry = get_package_install(ctx, logger, package)?;

    let effective_hash = {
        let mut hasher = Xxh3::new();
        package.get_package_meta_hash().hash(&mut hasher);
        hash_directory(install_store_entry.path(), &mut hasher)?;
        hasher.digest128()
    };

    if let Some(store_entry) = StoreEntry::get(&ctx.store, "pkg", effective_hash)? {
        ctx.ledger.record("pkg", pkg_hash, effective_hash)?;
        return Ok(store_entry);
    }

    let runtime_deps = package
        .runtime_dependencies
        .iter()
        .map(|pkg| format!("{}>={}_{}", pkg.name, pkg.version, pkg.revision)) // TODO: this is xbps specific and should be done in xbps.rs somehow
        .collect::<Vec<_>>();

    let workdir = WorkDirectory::create(&ctx.workdir_parent)?;
    package_create(
        ctx,
        &package.name,
        &package.version,
        package.revision,
        package.get_arch(),
        runtime_deps.iter().map(|str| str.as_str()).collect(),
        &install_store_entry.path(),
        &workdir.path(),
        logger,
    )?;

    let store_entry = StoreEntry::from_workdir(&ctx.store, workdir, "pkg", effective_hash)?;
    ctx.ledger.record("pkg", pkg_hash, effective_hash)?;

    Ok(store_entry)
}

fn get_package_install(ctx: &CoreContext, logger: &mut dyn Write, package: &Package) -> Result<StoreEntry, ProcessPackageError> {
    let pkg_content_hash = package.get_content_hash();

    if let Some(effective_hash) = ctx.ledger.lookup("install", pkg_content_hash)?
        && let Some(store_entry) = StoreEntry::get(&ctx.store, "install", effective_hash)?
    {
        return Ok(store_entry);
    }

    let root_pkgset = CachedPkgSet::get(&ctx.rootfs, &None, &package.global_env.global_native_packages, logger)?;
    let pkgset = CachedPkgSet::get(&ctx.rootfs, &root_pkgset, &package.dependencies.native, logger)?;
    let exec_env = ExecEnv::create(
        ctx,
        logger,
        pkgset,
        &package.dependencies.sources,
        &package.dependencies.packages,
        &package.dependencies.tools,
    )
    .map_err(|err| Box::new(err))?;

    let effective_hash = {
        let mut hasher = Xxh3::new();
        package.get_content_base_hash().hash(&mut hasher);
        exec_env.compute_deps_hash()?.hash(&mut hasher);
        hasher.digest128()
    };

    if let Some(store_entry) = StoreEntry::get(&ctx.store, "install", effective_hash)? {
        ctx.ledger.record("install", pkg_content_hash, effective_hash)?;
        return Ok(store_entry);
    }

    let mut _build_cachedir = None;
    let mut _build_workdir = None;
    let build_dir_path = if ctx
        .build_cache_enabled
        .iter()
        .any(|(platform, name)| platform == &package.platform && name == &package.name)
    {
        let build_dir = BuildDirectory::get(&ctx.build_cache, package.platform, package.get_arch(), &package.name)?;
        let path = build_dir.path();
        _build_cachedir = Some(build_dir);
        path
    } else {
        let workdir = WorkDirectory::create(&ctx.workdir_parent)?;
        let path = workdir.path();
        _build_workdir = Some(workdir);
        path
    };

    let build_mount = Mount {
        dest: PathBuf::from("/chariot/build"),
        kind: MountKind::Bind {
            from: build_dir_path,
            read_only: false,
            is_file: false,
        },
    };

    let source_dir = if package.dependencies.sources.iter().any(|(k, _)| k == &package.name) {
        Some(
            PathBuf::from(EXECENV_SOURCES_DIRECTORY_PATH)
                .join(&package.name)
                .to_string_lossy()
                .to_string(),
        )
    } else {
        None
    };

    let base_env = package
        .global_env
        .global_environment_variables
        .iter()
        .chain(&package.environment_variables)
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .chain([
            ("BUILD_DIR", "/chariot/build"),
            ("PREFIX", package.get_prefix()),
            ("ARCH", package.get_arch()),
        ])
        .chain(match &source_dir {
            Some(dir) => Some(("SOURCE_DIR", dir.as_str())),
            None => None,
        })
        .collect();

    if let Some(configure) = &package.configure {
        let exit_code = exec_env.exec(
            "/chariot/build",
            vec![&build_mount],
            &base_env,
            false,
            Some(logger),
            StderrTarget::Merge,
            configure.command(),
        )?;

        if exit_code != 0 {
            return Err(ProcessPackageError::Configure(exit_code));
        }
    }

    if let Some(build) = &package.build {
        let exit_code = exec_env.exec(
            "/chariot/build",
            vec![&build_mount],
            &base_env,
            false,
            Some(logger),
            StderrTarget::Merge,
            build.command(),
        )?;

        if exit_code != 0 {
            return Err(ProcessPackageError::Build(exit_code));
        }
    }

    let install_workdir = WorkDirectory::create(&ctx.workdir_parent)?;

    let exit_code = exec_env.exec(
        "/chariot/build",
        vec![
            &build_mount,
            &Mount {
                dest: PathBuf::from("/chariot/install"),
                kind: MountKind::Bind {
                    from: install_workdir.path(),
                    read_only: false,
                    is_file: false,
                },
            },
        ],
        &base_env.into_iter().chain([("INSTALL_DIR", "/chariot/install")]).collect(),
        false,
        Some(logger),
        StderrTarget::Merge,
        package.install.command(),
    )?;

    if exit_code != 0 {
        return Err(ProcessPackageError::Install(exit_code));
    }

    let store_entry = StoreEntry::from_workdir(&ctx.store, install_workdir, "install", effective_hash)?;
    ctx.ledger.record("install", pkg_content_hash, effective_hash)?;

    Ok(store_entry)
}
