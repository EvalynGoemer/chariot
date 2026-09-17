use std::{
    collections::HashMap,
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use chariot_rootfs::{CachedPkgSet, RootFS};
use chariot_runtime::{Mount, MountKind, RuntimeError};
use chariot_util::fs::FileSystemError;
use thiserror::Error;

use crate::{
    CoreContext,
    config::{
        script::Script,
        source::{Archive, ArchiveCompression, ArchiveKind},
    },
    workdir::WorkDirectory,
};

#[derive(Debug, Error)]
pub enum ArchiveFetchError {
    #[error(transparent)]
    Runtime(#[from] RuntimeError),

    #[error(transparent)]
    FileSystem(#[from] FileSystemError),

    #[error("Failed to download source archive")]
    Download,

    #[error("Checksum validation for archive failed")]
    ChecksumMismatch,

    #[error("Failed to extract source archive")]
    Extract,
}

pub fn fetch_archive(ctx: &CoreContext, logger: &mut dyn Write, archive: &Archive) -> Result<WorkDirectory, ArchiveFetchError> {
    let download_dir = WorkDirectory::create(&ctx.workdir_parent)?;
    let archive_path = download_dir.path().join("archive");

    File::create(&archive_path).map_err(|err| FileSystemError::CreateFile {
        path: archive_path.clone(),
        source: err,
    })?;

    download_archive(
        &ctx.rootfs,
        ctx.wget_pkgset.as_deref(),
        ctx.sha256sum_pkgset.as_deref(),
        logger,
        &archive.url,
        &archive.checksum,
        &archive_path,
    )?;

    let work_directory = WorkDirectory::create(&ctx.workdir_parent)?;
    extract_archive(
        &ctx.rootfs,
        ctx.bsdtar_pkgset.as_deref(),
        logger,
        &archive.kind,
        &archive.compression,
        &archive_path,
        &work_directory.path(),
    )?;

    Ok(work_directory)
}

pub fn download_archive(
    rootfs: &Arc<RootFS>,
    wget_pkgset: Option<&CachedPkgSet>,
    sha256sum_pkgset: Option<&CachedPkgSet>,
    logger: &mut dyn Write,
    url: &str,
    checksum: &str,
    dest: &Path,
) -> Result<(), ArchiveFetchError> {
    let archive_binding = Mount {
        dest: PathBuf::from("/chariot/archive"),
        kind: MountKind::Bind {
            from: dest.to_path_buf(),
            read_only: false,
            is_file: true,
        },
    };

    let exit_code = rootfs.exec(
        "/",
        &vec![&archive_binding],
        &HashMap::from([("ARCHIVE_URL", url)]),
        logger,
        Script::bash("wget --no-hsts -qO /chariot/archive \"$ARCHIVE_URL\"").command(),
        wget_pkgset,
        None,
    )?;

    if exit_code != 0 {
        return Err(ArchiveFetchError::Download);
    }

    let exit_code = rootfs.exec(
        "/",
        &vec![&archive_binding],
        &HashMap::from([("ARCHIVE_CHECKSUM", checksum)]),
        logger,
        Script::bash("echo \"$ARCHIVE_CHECKSUM  /chariot/archive\n\" | sha256sum -c -").command(),
        sha256sum_pkgset,
        None,
    )?;

    if exit_code != 0 {
        return Err(ArchiveFetchError::ChecksumMismatch);
    }

    Ok(())
}

pub fn extract_archive(
    rootfs: &Arc<RootFS>,
    bsdtar_pkgset: Option<&CachedPkgSet>,
    logger: &mut dyn Write,
    kind: &ArchiveKind,
    compression: &ArchiveCompression,
    src: &Path,
    dest: &Path,
) -> Result<(), ArchiveFetchError> {
    match kind {
        ArchiveKind::Tar => {}
    }

    let compression_flag = match compression {
        ArchiveCompression::Gzip => "--gzip",
        ArchiveCompression::Xz => "--xz",
        ArchiveCompression::Bzip2 => "--bzip2",
    };

    let exit_code = rootfs.exec(
        "/",
        &vec![
            &Mount {
                dest: PathBuf::from("/chariot/source"),
                kind: MountKind::Bind {
                    from: src.to_path_buf(),
                    read_only: true,
                    is_file: true,
                },
            },
            &Mount {
                dest: PathBuf::from("/chariot/dest"),
                kind: MountKind::Bind {
                    from: dest.to_path_buf(),
                    read_only: false,
                    is_file: false,
                },
            },
        ],
        &HashMap::<&str, &str>::new(),
        logger,
        Script::bash(format!(
            "bsdtar --no-same-owner --strip-components 1 -x {} -C /chariot/dest -f /chariot/source",
            compression_flag
        ))
        .command(),
        bsdtar_pkgset,
        None,
    )?;

    if exit_code != 0 {
        return Err(ArchiveFetchError::Extract);
    }

    Ok(())
}
