use std::{collections::HashMap, io::Write};

use chariot_rootfs::{CachedPkgSet, GetPkgSetError};
use chariot_util::fs::FileSystemError;
use thiserror::Error;

use crate::{
    CoreContext, HOST_ARCH, NOARCH_ARCH,
    cache::{StoreEntry, WorkDirectory},
    config::{
        Dependencies,
        package::{Package, PackagePlatform},
    },
    execenv::ExecEnv,
    package::{ProcessPackageError, process_package},
    source::fetch_source,
    xbps::{XBPSPackageInstallError, package_install},
};

#[derive(Debug, Error)]
pub enum ResolveDependenciesError {
    #[error(transparent)]
    FileSystem(#[from] FileSystemError),

    #[error(transparent)]
    ProcessPackage(#[from] ProcessPackageError),

    #[error(transparent)]
    GetPkgSet(#[from] GetPkgSetError),

    #[error(transparent)]
    PackageInstall(#[from] XBPSPackageInstallError),
}

pub fn resolve_repos_for_pkg(ctx: &CoreContext, logger: &mut dyn Write, pkg: &Package) -> Result<Vec<StoreEntry>, ProcessPackageError> {
    let mut entries = vec![process_package(ctx, logger, pkg)?];

    for rdep in &pkg.runtime_dependencies {
        assert!(rdep.platform == pkg.platform);
        entries.extend(resolve_repos_for_pkg(ctx, logger, rdep)?);
    }

    Ok(entries)
}

pub fn resolve_dependencies<'a>(
    ctx: &'a CoreContext,
    logger: &mut dyn Write,
    dependencies: &Dependencies,
) -> Result<ExecEnv<'a>, ResolveDependenciesError> {
    let mut cached_source_deps = HashMap::new();
    for (name, source) in &dependencies.sources {
        cached_source_deps.insert(
            name.clone(),
            fetch_source(ctx, logger, source).map_err(|err| ProcessPackageError::SourceFetch {
                name: name.clone(),
                source: err,
            })?,
        );
    }

    let pkgset = CachedPkgSet::get(
        &ctx.rootfs,
        ctx.root_pkgset.clone(),
        &dependencies.native.iter().map(|str| str.as_str()).collect(),
        logger,
    )?;

    let sysroot_workdir = WorkDirectory::create(&ctx.cache)?;
    for pkg in &dependencies.packages {
        assert!(pkg.platform == PackagePlatform::Target);
        let entries = resolve_repos_for_pkg(ctx, logger, pkg)?;
        package_install(
            ctx,
            &pkg.name,
            &pkg.version,
            pkg.revision,
            if pkg.subscribed_options.contains("arch")
                && let Some(arch) = pkg.config_env.effective_options.get("arch")
            {
                arch
            } else {
                NOARCH_ARCH
            },
            entries.iter().map(|entry| entry.path()).collect(),
            &sysroot_workdir.path(),
            false,
            logger,
        )?;
    }

    let tool_overlay_workdir = if dependencies.tools.len() > 0 {
        let tool_overlay_workdir = WorkDirectory::create(&ctx.cache)?;
        for tool in &dependencies.tools {
            assert!(tool.platform == PackagePlatform::Host);
            let entries = resolve_repos_for_pkg(ctx, logger, tool)?;
            package_install(
                ctx,
                &tool.name,
                &tool.version,
                tool.revision,
                HOST_ARCH,
                entries.iter().map(|entry| entry.path()).collect(),
                &tool_overlay_workdir.path(),
                true,
                logger,
            )?;
        }
        Some(tool_overlay_workdir)
    } else {
        None
    };

    Ok(ExecEnv {
        ctx,
        pkgset,
        sources: cached_source_deps,
        sysroot: sysroot_workdir,
        tool_overlay: tool_overlay_workdir,
    })
}
