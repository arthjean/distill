use crate::surface::SurfaceError;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

pub(super) const ABSENT_MARKER: &[u8] = b"configuration did not exist before Distill setup\n";

pub(super) struct Restore {
    pub action: &'static str,
    pub backup_path: PathBuf,
}

pub(super) fn restore(path: &Path) -> Result<Restore, SurfaceError> {
    let current = read_existing(path)?;
    let backup = backup_path(path);
    let absent = absent_path(path);
    let backup_bytes = read_existing(&backup)?;
    let absent_bytes = read_existing(&absent)?;
    let action = match (backup_bytes, absent_bytes) {
        (Some(bytes), None) => {
            write(path, &bytes)?;
            "restored"
        }
        (None, Some(marker)) if marker == ABSENT_MARKER => {
            if current.is_some() {
                fs::remove_file(path)
                    .map_err(|_| SurfaceError::invalid("cannot restore absent configuration"))?;
            }
            "restored_absent"
        }
        (None, None) => {
            return Err(SurfaceError::invalid(
                "no Distill configuration backup exists",
            ));
        }
        _ => {
            return Err(SurfaceError::invalid(
                "Distill configuration backup state is ambiguous",
            ));
        }
    };
    Ok(Restore {
        action,
        backup_path: backup,
    })
}

pub(super) fn create(path: &Path, original: Option<&[u8]>) -> Result<(), SurfaceError> {
    let backup = backup_path(path);
    let absent = absent_path(path);
    let backup_bytes = read_existing(&backup)?;
    let absent_bytes = read_existing(&absent)?;
    match (original, backup_bytes, absent_bytes) {
        (Some(_), Some(_), None) => Ok(()),
        (Some(_), None, Some(marker)) if marker == ABSENT_MARKER => Ok(()),
        (None, None, Some(marker)) if marker == ABSENT_MARKER => Ok(()),
        (Some(bytes), None, None) => write(&backup, bytes),
        (None, None, None) => write(&absent, ABSENT_MARKER),
        _ => Err(SurfaceError::invalid(
            "Distill configuration backup state is ambiguous",
        )),
    }
}

pub(super) fn read_existing(path: &Path) -> Result<Option<Vec<u8>>, SurfaceError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(SurfaceError::invalid(
            "refusing to edit a symlinked configuration",
        )),
        Ok(metadata) if !metadata.is_file() => Err(SurfaceError::invalid(
            "configuration path is not a regular file",
        )),
        Ok(_) => fs::read(path)
            .map(Some)
            .map_err(|_| SurfaceError::invalid("cannot read configuration")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(SurfaceError::invalid("cannot inspect configuration")),
    }
}

pub(super) fn write(path: &Path, bytes: &[u8]) -> Result<(), SurfaceError> {
    let parent = path
        .parent()
        .ok_or_else(|| SurfaceError::invalid("configuration path has no parent"))?;
    fs::create_dir_all(parent)
        .map_err(|_| SurfaceError::invalid("cannot create configuration directory"))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| SurfaceError::invalid("configuration filename is invalid"))?;
    let temporary = parent.join(format!(".{name}.distill-tmp-{}", std::process::id()));
    let mut file = open_private_temporary(&temporary)?;
    let write_result = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| SurfaceError::invalid("cannot write configuration atomically"));
    if let Err(error) = write_result {
        let _cleanup = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = set_private_mode(&temporary) {
        let _cleanup = fs::remove_file(&temporary);
        return Err(error);
    }
    fs::rename(&temporary, path).map_err(|_| {
        let _cleanup = fs::remove_file(&temporary);
        SurfaceError::invalid("cannot replace configuration atomically")
    })?;
    Ok(())
}

#[cfg(unix)]
fn open_private_temporary(path: &Path) -> Result<fs::File, SurfaceError> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| SurfaceError::invalid("cannot create atomic configuration file"))
}

#[cfg(not(unix))]
fn open_private_temporary(_path: &Path) -> Result<fs::File, SurfaceError> {
    Err(SurfaceError::invalid(
        "configuration permission enforcement is unsupported on this platform",
    ))
}

#[cfg(unix)]
fn set_private_mode(path: &Path) -> Result<(), SurfaceError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|_| SurfaceError::invalid("cannot enforce configuration permissions"))
}

#[cfg(not(unix))]
fn set_private_mode(_path: &Path) -> Result<(), SurfaceError> {
    Err(SurfaceError::invalid(
        "configuration permission enforcement is unsupported on this platform",
    ))
}

pub(super) fn backup_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{}.distill-backup",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config")
    ))
}

pub(super) fn absent_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{}.distill-backup-absent",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config")
    ))
}
