use std::{
    fs::{self, File},
    hash::Hasher,
    io::Read,
    os::unix::{ffi::OsStrExt, fs::FileTypeExt},
    path::Path,
};

use crate::fs::{FileSystemError, dir_entries};

fn write_header<H: Hasher>(hasher: &mut H, tag: u8, rel_path: &[u8]) {
    hasher.write_u8(tag);
    hasher.write_u64(rel_path.len() as u64);
    hasher.write(rel_path);
}

fn walk<H: Hasher>(hasher: &mut H, dir: &Path, rel: &[u8]) -> Result<(), FileSystemError> {
    let mut entries = dir_entries(dir)?;
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let name = entry.file_name();
        let name_bytes = name.as_bytes();

        let mut rel_path = Vec::with_capacity(rel.len() + 1 + name_bytes.len());
        rel_path.extend_from_slice(rel);
        if !rel.is_empty() {
            rel_path.push(b'/');
        }
        rel_path.extend_from_slice(name_bytes);

        let path = entry.path();
        let file_type = entry.file_type().map_err(|err| FileSystemError::FileType {
            path: path.clone(),
            source: err,
        })?;

        if file_type.is_dir() {
            write_header(hasher, b'D', &rel_path);
            walk(hasher, &path, &rel_path)?;
        } else if file_type.is_symlink() {
            write_header(hasher, b'L', &rel_path);
            let target = fs::read_link(&path).map_err(|err| FileSystemError::ReadLink {
                path: path.to_path_buf(),
                source: err,
            })?;
            let target_bytes = target.as_os_str().as_bytes();
            hasher.write_u64(target_bytes.len() as u64);
            hasher.write(target_bytes);
        } else if file_type.is_file() {
            write_header(hasher, b'F', &rel_path);
            let meta = entry.metadata().map_err(|err| FileSystemError::Metadata {
                path: path.clone(),
                source: err,
            })?;
            hasher.write_u64(meta.len());
            let mut file = File::open(&path).map_err(|err| FileSystemError::Open {
                path: path.clone(),
                source: err,
            })?;
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = file.read(&mut buf).map_err(|err| FileSystemError::ReadFile {
                    path: path.clone(),
                    source: err,
                })?;
                if n == 0 {
                    break;
                }
                hasher.write(&buf[..n]);
            }
        } else if file_type.is_socket() {
            write_header(hasher, b'S', &rel_path);
        } else if file_type.is_fifo() {
            write_header(hasher, b'I', &rel_path);
        } else if file_type.is_block_device() {
            write_header(hasher, b'B', &rel_path);
        } else if file_type.is_char_device() {
            write_header(hasher, b'C', &rel_path);
        } else {
            write_header(hasher, b'O', &rel_path);
        }
    }

    Ok(())
}

pub fn hash_directory<H: Hasher>(path: impl AsRef<Path>, hasher: &mut H) -> Result<(), FileSystemError> {
    walk(hasher, path.as_ref(), &[])
}
