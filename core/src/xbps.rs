use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
};

use chariot_rootfs::RootFSOverlay;
use chariot_runtime::{Mount, MountKind, RuntimeError};
use chariot_util::fs::FileSystemError;
use thiserror::Error;

use crate::{CoreContext, cache::WorkDirectory};

#[derive(Debug, Error)]
pub enum XBPSValidationError {
    #[error("Package name `{}` cannot contain whitespace", .0)]
    PackageName(String),

    #[error("Package version `{}` cannot contain whitespace, `_`, or `-`", .0)]
    PackageVersion(String),

    #[error("Arch `{}` cannot contain whitespace", .0)]
    Arch(String),

    #[error("Runtime dependency `{}` cannot contain whitespace", .0)]
    RuntimeDependency(String),
}

#[derive(Debug, Error)]
pub enum XBPSPackageCreateError {
    #[error(transparent)]
    Runtime(#[from] RuntimeError),

    #[error(transparent)]
    Validation(#[from] XBPSValidationError),

    #[error("xbps-create exited with non-zero exit code {}", .0)]
    CreateError(i32),

    #[error("xbps-rindex exited with non-zero exit code {}", .0)]
    RepoIndexError(i32),
}

#[derive(Debug, Error)]
pub enum XBPSPackageInstallError {
    #[error(transparent)]
    FileSystem(#[from] FileSystemError),

    #[error(transparent)]
    Runtime(#[from] RuntimeError),

    #[error(transparent)]
    Validation(#[from] XBPSValidationError),

    #[error("xbps-install exited with non-zero exit code {}", .0)]
    Error(i32),
}

fn validate_package_name(name: &str) -> Result<(), XBPSValidationError> {
    if name.chars().any(|c| c.is_whitespace()) {
        return Err(XBPSValidationError::PackageName(name.to_string()));
    }
    Ok(())
}

fn validate_package_version(version: &str) -> Result<(), XBPSValidationError> {
    if version.chars().any(|c| c.is_whitespace() || c == '_' || c == '-') {
        return Err(XBPSValidationError::PackageVersion(version.to_string()));
    }
    Ok(())
}

fn validate_arch(arch: &str) -> Result<(), XBPSValidationError> {
    if arch.chars().any(|c| c.is_whitespace()) {
        return Err(XBPSValidationError::Arch(arch.to_string()));
    }
    Ok(())
}

pub fn package_create(
    ctx: &CoreContext,
    name: &str,
    version: &str,
    revision: usize,
    arch: &str,
    runtime_dependencies: Vec<&str>,
    from_dir: &Path,
    dest_dir: &Path,
    logger: &mut dyn Write,
) -> Result<(), XBPSPackageCreateError> {
    validate_package_name(name)?;
    validate_package_version(version)?;
    validate_arch(arch)?;

    for rdep in &runtime_dependencies {
        if rdep.chars().any(|c| c.is_whitespace()) {
            return Err(XBPSPackageCreateError::Validation(XBPSValidationError::RuntimeDependency(
                rdep.to_string(),
            )));
        }
    }

    let exit_code = ctx.rootfs.exec(
        "/chariot/xbps/dest",
        &vec![
            &Mount {
                dest: PathBuf::from("/chariot/xbps/dest"),
                kind: MountKind::Bind {
                    from: dest_dir.to_path_buf(),
                    read_only: false,
                    is_file: false,
                },
            },
            &Mount {
                dest: PathBuf::from("/chariot/xbps/package"),
                kind: MountKind::Bind {
                    from: from_dir.to_path_buf(),
                    read_only: true,
                    is_file: false,
                },
            },
        ],
        &HashMap::from([
            ("XBPS_ARCH", "invalid"),
            ("XBPS_TARGET_ARCH", arch),
            ("PKG_NAME", name),
            ("PKG_VER", version),
            ("PKG_REV", revision.to_string().as_str()),
            ("PKG_RDEPS", runtime_dependencies.join(" ").as_str())
        ]),
        logger,
        vec![
            "bash",
            "-c",
            format!("xbps-create --built-with chariot --architecture \"$XBPS_TARGET_ARCH\" --pkgver \"$PKG_NAME-${{PKG_VER}}_$PKG_REV\" --desc \"Package $PKG_NAME built by chariot\" --dependencies \"$PKG_RDEPS\" /chariot/xbps/package").as_str(),
        ],
        None,
        None,
    )?;

    if exit_code != 0 {
        return Err(XBPSPackageCreateError::CreateError(exit_code));
    }

    let exit_code = ctx.rootfs.exec(
        "/chariot/xbps/repo",
        &vec![&Mount {
            dest: PathBuf::from("/chariot/xbps/repo"),
            kind: MountKind::Bind {
                from: dest_dir.to_path_buf(),
                read_only: false,
                is_file: false,
            },
        }],
        &HashMap::from([
            ("XBPS_ARCH", "invalid"),
            ("XBPS_TARGET_ARCH", arch),
            ("PKG_NAME", name),
            ("PKG_VER", version),
            ("PKG_REV", revision.to_string().as_str()),
        ]),
        logger,
        vec![
            "bash",
            "-c",
            format!("xbps-rindex -f -a \"$PKG_NAME-${{PKG_VER}}_$PKG_REV.$XBPS_TARGET_ARCH.xbps\"").as_str(),
        ],
        None,
        None,
    )?;

    if exit_code != 0 {
        return Err(XBPSPackageCreateError::RepoIndexError(exit_code));
    }

    Ok(())
}

pub fn package_install(
    ctx: &CoreContext,
    name: &str,
    version: &str,
    revision: usize,
    arch: &str,
    repo_dirs: Vec<PathBuf>,
    dest_dir: &Path,
    dest_root_overlay: bool,
    logger: &mut dyn Write,
) -> Result<(), XBPSPackageInstallError> {
    validate_package_name(name)?;
    validate_package_version(version)?;
    validate_arch(arch)?;

    let repo_mounts = repo_dirs
        .into_iter()
        .enumerate()
        .map(|(i, repo_dir)| Mount {
            dest: PathBuf::from(format!("/chariot/xbps/repo{}", i)),
            kind: MountKind::Bind {
                from: repo_dir,
                read_only: true,
                is_file: false,
            },
        })
        .collect::<Vec<_>>();

    let dest_mount = match dest_root_overlay {
        true => None,
        false => Some(Mount {
            dest: PathBuf::from("/chariot/xbps/install"),
            kind: MountKind::Bind {
                from: dest_dir.to_path_buf(),
                read_only: false,
                is_file: false,
            },
        }),
    };

    let mut _workdir = None;
    let rootfs_overlay = match dest_root_overlay {
        true => {
            let overlay_workdir = WorkDirectory::create(&ctx.cache)?;
            let overlay = RootFSOverlay::ReadWrite {
                path: dest_dir.to_path_buf(),
                work_path: overlay_workdir.path(),
            };
            _workdir = Some(overlay_workdir);
            Some(overlay)
        }
        false => None,
    };

    let exit_code = ctx.rootfs.exec(
        "/",
        &repo_mounts.iter().chain(&dest_mount).collect(),
        &HashMap::from([
            ("XBPS_ARCH", "invalid"),
            ("XBPS_TARGET_ARCH", arch),
            ("PKG_NAME", name),
            ("PKG_VER", version),
            ("PKG_REV", revision.to_string().as_str()),
        ]),
        logger,
        vec![
            "bash",
            "-c",
            format!(
                "xbps-install --reproducible --yes --rootdir {} {} \"$PKG_NAME-${{PKG_VER}}_$PKG_REV\"",
                if dest_root_overlay { "/" } else { "/chariot/xbps/install" },
                repo_mounts
                    .iter()
                    .map(|mnt| format!("--repository {}", mnt.dest.display()))
                    .collect::<Vec<_>>()
                    .join(" "),
            )
            .as_str(),
        ],
        None,
        rootfs_overlay,
    )?;

    if exit_code != 0 {
        return Err(XBPSPackageInstallError::Error(exit_code));
    }

    Ok(())
}
