use std::{collections::HashMap, io::Write, path::PathBuf};

use chariot_runtime::{Mount, MountKind, RuntimeError};
use chariot_util::fs::FileSystemError;
use thiserror::Error;

use crate::{
    CoreContext, HOST_ARCH, NOARCH_ARCH,
    cache::{StoreEntry, WorkDirectory},
    config::package::{Package, PackagePlatform},
    dependencies::{ResolveDependenciesError, resolve_dependencies},
    source::SourceFetchError,
    xbps::{XBPSPackageCreateError, package_create},
};

#[derive(Debug, Error)]
pub enum ProcessPackageError {
    #[error(transparent)]
    ResolveDependencies(#[from] Box<ResolveDependenciesError>), // TODO: this box is nasty

    #[error(transparent)]
    FileSystem(#[from] FileSystemError),

    #[error(transparent)]
    Runtime(#[from] RuntimeError),

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

pub fn process_package(ctx: &CoreContext, logger: &mut dyn Write, package: &Package) -> Result<StoreEntry, ProcessPackageError> {
    let pkg_hash = package.get_package_hash();
    if let Some(store_entry) = StoreEntry::get(&ctx.cache, "pkg", pkg_hash)? {
        return Ok(store_entry);
    }

    let install_store_entry = get_package_install(ctx, logger, package)?;

    let runtime_deps = package
        .runtime_dependencies
        .iter()
        .map(|pkg| format!("{}>={}_{}", pkg.name, pkg.version, pkg.revision))
        .collect::<Vec<_>>();

    let workdir = WorkDirectory::create(&ctx.cache)?;
    package_create(
        ctx,
        &package.name,
        &package.version,
        package.revision,
        match package.platform {
            PackagePlatform::Target => {
                if package.subscribed_options.contains("arch")
                    && let Some(arch) = package.config_env.effective_options.get("arch")
                {
                    arch
                } else {
                    NOARCH_ARCH
                }
            }
            PackagePlatform::Host => HOST_ARCH,
        },
        runtime_deps.iter().map(|str| str.as_str()).collect(),
        &install_store_entry.path(),
        &workdir.path(),
        logger,
    )?;

    Ok(workdir.move_to_store("pkg", pkg_hash)?)
}

fn get_package_install(ctx: &CoreContext, logger: &mut dyn Write, package: &Package) -> Result<StoreEntry, ProcessPackageError> {
    let pkg_content_hash = package.get_content_hash();

    if let Some(store_entry) = StoreEntry::get(&ctx.cache, "install", pkg_content_hash)? {
        return Ok(store_entry);
    }

    let exec_env = resolve_dependencies(ctx, logger, &package.dependencies).map_err(|err| Box::new(err))?;

    let build_workdir = WorkDirectory::create(&ctx.cache)?;

    let build_mount = Mount {
        dest: PathBuf::from("/chariot/build"),
        kind: MountKind::Bind {
            from: build_workdir.path(),
            read_only: false,
            is_file: false,
        },
    };

    let mut base_env = HashMap::from([
        ("BUILD_DIR", "/chariot/build"),
        (
            "PREFIX",
            match package.platform {
                PackagePlatform::Host => "/usr/local",
                PackagePlatform::Target => &package.config_env.target_prefix,
            },
        ),
    ]);

    let active_options = package.config_env.resolve_subscribed_options(&package.subscribed_options);
    let option_environment_vars = active_options.iter().map(|(k, v)| (format!("OPTION_{}", k), v)).collect::<Vec<_>>();

    for (k, v) in &option_environment_vars {
        base_env.insert(k, v);
    }

    if let Some(configure) = &package.configure {
        let exit_code = exec_env.exec("/chariot/build", vec![&build_mount], &base_env, logger, configure.command())?;

        if exit_code != 0 {
            return Err(ProcessPackageError::Configure(exit_code));
        }
    }

    if let Some(build) = &package.build {
        let exit_code = exec_env.exec("/chariot/build", vec![&build_mount], &base_env, logger, build.command())?;

        if exit_code != 0 {
            return Err(ProcessPackageError::Build(exit_code));
        }
    }

    let install_workdir = WorkDirectory::create(&ctx.cache)?;

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
        logger,
        package.install.command(),
    )?;

    if exit_code != 0 {
        return Err(ProcessPackageError::Install(exit_code));
    }

    Ok(install_workdir.move_to_store("install", pkg_content_hash)?)
}
