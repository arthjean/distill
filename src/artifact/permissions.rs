use crate::types::{Failure, FailureCode};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub(super) fn secure_store_root(path: &Path) -> Result<(), Failure> {
    match fs::symlink_metadata(path) {
        Ok(_) => return validate_store_root(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => {
            return Err(Failure::new(
                FailureCode::PermissionDenied,
                "artifact store root cannot be inspected",
            ));
        }
    }
    fs::create_dir_all(path).map_err(|_| {
        Failure::new(
            FailureCode::PermissionDenied,
            "artifact store root cannot be created",
        )
    })?;
    inspect_store_root(path)?;
    set_mode(path, 0o700)
}

pub(super) fn validate_store_root(path: &Path) -> Result<(), Failure> {
    let metadata = inspect_store_root(path)?;
    if store_mode(&metadata) != 0o700 {
        return Err(Failure::new(
            FailureCode::PermissionDenied,
            "existing artifact store root is not mode 0700",
        ));
    }
    Ok(())
}

fn inspect_store_root(path: &Path) -> Result<fs::Metadata, Failure> {
    let metadata = fs::symlink_metadata(path).map_err(|_| {
        Failure::new(
            FailureCode::PermissionDenied,
            "artifact store root cannot be inspected",
        )
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Failure::new(
            FailureCode::UnsafeRoot,
            "artifact store root must be a real directory",
        ));
    }
    validate_store_owner(&metadata, "artifact store root")?;
    verify_no_store_symlinks(path)?;
    Ok(metadata)
}

#[cfg(unix)]
fn validate_store_owner(metadata: &fs::Metadata, label: &str) -> Result<(), Failure> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions and does not dereference memory.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(Failure::new(
            FailureCode::PermissionDenied,
            format!("{label} is not owned by the current user"),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_store_owner(_metadata: &fs::Metadata, _label: &str) -> Result<(), Failure> {
    Err(Failure::new(
        FailureCode::PermissionDenied,
        "POSIX ownership enforcement is unavailable",
    ))
}

fn verify_no_store_symlinks(path: &Path) -> Result<(), Failure> {
    let mut current = PathBuf::from("/");
    for component in path.components() {
        match component {
            std::path::Component::RootDir => continue,
            std::path::Component::Normal(value) => current.push(value),
            _ => {
                return Err(Failure::new(
                    FailureCode::UnsafeRoot,
                    "artifact store root contains an unsafe component",
                ));
            }
        }
        if fs::symlink_metadata(&current)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(Failure::new(
                FailureCode::UnsafeRoot,
                "artifact store root traverses a symlink",
            ));
        }
    }
    Ok(())
}

fn validate_store_file(path: &Path) -> Result<(), Failure> {
    let metadata = fs::symlink_metadata(path).map_err(|_| {
        Failure::new(
            FailureCode::PermissionDenied,
            "artifact store file cannot be inspected",
        )
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(Failure::new(
            FailureCode::UnsafeRoot,
            "artifact store file must be a regular file",
        ));
    }
    validate_store_owner(&metadata, "artifact store file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(Failure::new(
                FailureCode::UnsafeRoot,
                "artifact store file must not be hard-linked",
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_optional_store_file(path: &Path) -> Result<(), Failure> {
    match fs::symlink_metadata(path) {
        Ok(_) => validate_store_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(Failure::new(
            FailureCode::PermissionDenied,
            "artifact store file cannot be inspected",
        )),
    }
}

pub(super) fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

#[cfg(unix)]
fn store_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn store_mode(_metadata: &fs::Metadata) -> u32 {
    0
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), Failure> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|_| {
        Failure::new(
            FailureCode::PermissionDenied,
            "artifact store permissions cannot be enforced",
        )
    })?;
    let actual = fs::metadata(path)
        .map_err(|_| {
            Failure::new(
                FailureCode::PermissionDenied,
                "artifact store permissions cannot be verified",
            )
        })?
        .permissions()
        .mode()
        & 0o777;
    if actual != mode {
        return Err(Failure::new(
            FailureCode::PermissionDenied,
            "artifact store permissions are not private",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), Failure> {
    Err(Failure::new(
        FailureCode::PermissionDenied,
        "POSIX permission enforcement is unavailable",
    ))
}

pub(super) fn enforce_store_modes(path: &Path) -> Result<(), Failure> {
    validate_store_file(path)?;
    set_mode(path, 0o600)?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = sidecar_path(path, suffix);
        if fs::symlink_metadata(&sidecar).is_ok() {
            validate_store_file(&sidecar)?;
            set_mode(&sidecar, 0o600)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn sidecar_paths_preserve_non_utf8_store_names() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let path = PathBuf::from(std::ffi::OsString::from_vec(
            b"/tmp/distill-\xff.sqlite".to_vec(),
        ));
        assert_eq!(
            sidecar_path(&path, "-wal").as_os_str().as_bytes(),
            b"/tmp/distill-\xff.sqlite-wal"
        );
    }
}
