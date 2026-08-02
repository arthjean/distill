#[cfg(test)]
use crate::request_policy;
#[cfg(test)]
use crate::types::{Budget, CONTRACT_VERSION, CountUnit, Retention, Source};

mod process;
use crate::{
    request_policy::{LocalSource, MAX_SOURCE_BYTES, ProcessSource},
    types::{
        ByteString, EngineConfig, Failure, FailureCode, ProcessAcquisition, ValidatedAcquisition,
    },
};
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    collections::BTreeMap,
    ffi::{CString, OsStr, OsString},
    fs::{self, File},
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::MetadataExt,
            process::CommandExt,
        },
    },
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug)]
pub(crate) struct Acquired {
    pub bytes: Vec<u8>,
    pub receipt: ValidatedAcquisition,
}

impl Acquired {
    fn checked(
        bytes: Vec<u8>,
        receipt: Result<ValidatedAcquisition, &'static str>,
    ) -> Result<Self, AcquisitionError> {
        let receipt = receipt.map_err(|message| {
            AcquisitionError::clean(Failure::new(FailureCode::InvariantBreach, message))
        })?;
        Ok(Self { bytes, receipt })
    }
}

#[derive(Debug)]
pub(crate) struct AcquisitionError {
    pub failure: Failure,
    pub partial: Option<Acquired>,
}

impl AcquisitionError {
    fn clean(failure: Failure) -> Self {
        Self {
            failure,
            partial: None,
        }
    }
}

pub(crate) trait LocalRuntime: Send + Sync {
    fn acquire(&self, source: &LocalSource) -> Result<Acquired, AcquisitionError>;
    fn now(&self) -> Result<u64, Failure>;
    fn random_id(&self) -> Result<String, Failure>;
}

#[derive(Clone, Debug)]
struct ConfiguredRoot {
    path: PathBuf,
    device: u64,
    inode: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct ProductionRuntime {
    roots: Arc<BTreeMap<String, ConfiguredRoot>>,
    environment_profiles: Arc<BTreeMap<String, BTreeMap<String, String>>>,
    #[cfg(test)]
    request_config: EngineConfig,
}

impl ProductionRuntime {
    pub(crate) fn new(config: &EngineConfig) -> Result<Self, Failure> {
        let mut roots = BTreeMap::new();
        for (id, path) in &config.roots {
            if !crate::contract::valid_identifier(id) {
                return Err(Failure::new(
                    FailureCode::InvalidRequest,
                    "root ID is invalid",
                ));
            }
            let root = validate_root(path)?;
            roots.insert(id.clone(), root);
        }
        for (id, profile) in &config.environment_profiles {
            if !crate::contract::valid_identifier(id) {
                return Err(Failure::new(
                    FailureCode::InvalidRequest,
                    "environment profile ID is invalid",
                ));
            }
            if profile.len() > 128 {
                return Err(Failure::new(
                    FailureCode::InvalidRequest,
                    "environment profile has too many variables",
                ));
            }
            for (key, value) in profile {
                if key.is_empty()
                    || key.len() > 128
                    || value.len() > 4_096
                    || key.bytes().any(|byte| byte == b'=' || byte == 0)
                    || value.bytes().any(|byte| byte == 0)
                {
                    return Err(Failure::new(
                        FailureCode::InvalidRequest,
                        "environment profile contains an invalid variable",
                    ));
                }
            }
        }
        Ok(Self {
            roots: Arc::new(roots),
            environment_profiles: Arc::new(config.environment_profiles.clone()),
            #[cfg(test)]
            request_config: config.clone(),
        })
    }

    fn acquire_inline(bytes: &ByteString) -> Result<Acquired, AcquisitionError> {
        let bytes = bytes.0.clone();
        let receipt = ValidatedAcquisition::inline_complete(bytes.len() as u64);
        Ok(Acquired { bytes, receipt })
    }

    fn acquire_file(
        &self,
        root_id: &str,
        relative_path: &ByteString,
        reject_binary: bool,
    ) -> Result<Acquired, AcquisitionError> {
        let root = self.roots.get(root_id).ok_or_else(|| {
            AcquisitionError::clean(Failure::new(
                FailureCode::InvariantBreach,
                "validated file root is unavailable",
            ))
        })?;
        let mut file =
            open_beneath(root, &relative_path.0, false).map_err(AcquisitionError::clean)?;
        let before = file.metadata().map_err(|_| {
            AcquisitionError::clean(Failure::new(
                FailureCode::AcquisitionFailed,
                "cannot inspect the opened file",
            ))
        })?;
        if !before.is_file() {
            return Err(AcquisitionError::clean(Failure::new(
                FailureCode::AcquisitionFailed,
                "file source is not a regular file",
            )));
        }
        if before.len() > MAX_SOURCE_BYTES as u64 {
            return Err(AcquisitionError::clean(Failure::new(
                FailureCode::InputTooLarge,
                "file source exceeds the 10 MiB limit",
            )));
        }
        let mut bytes = Vec::with_capacity(before.len() as usize);
        file.by_ref()
            .take((MAX_SOURCE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| {
                AcquisitionError::clean(Failure::new(
                    FailureCode::AcquisitionFailed,
                    "file source could not be read",
                ))
            })?;
        if bytes.len() > MAX_SOURCE_BYTES {
            return Err(AcquisitionError::clean(Failure::new(
                FailureCode::InputTooLarge,
                "file source grew beyond the 10 MiB limit",
            )));
        }
        let after = file.metadata().map_err(|_| {
            AcquisitionError::clean(Failure::new(
                FailureCode::AcquisitionFailed,
                "cannot revalidate the opened file",
            ))
        })?;
        if before.dev() != after.dev()
            || before.ino() != after.ino()
            || before.len() != after.len()
            || before.mtime() != after.mtime()
            || before.mtime_nsec() != after.mtime_nsec()
        {
            return Err(AcquisitionError::clean(Failure::new(
                FailureCode::AcquisitionFailed,
                "file identity changed during capture",
            )));
        }
        if reject_binary && (bytes.contains(&0) || std::str::from_utf8(&bytes).is_err()) {
            return Err(AcquisitionError::clean(Failure::new(
                FailureCode::AcquisitionFailed,
                "binary file rejected by policy",
            )));
        }
        let source_bytes = bytes.len() as u64;
        let receipt = ValidatedAcquisition::file_complete(root_id, relative_path, source_bytes);
        Acquired::checked(bytes, receipt)
    }

    fn acquire_process(&self, source: &ProcessSource) -> Result<Acquired, AcquisitionError> {
        let root = self.roots.get(&source.cwd_root_id).ok_or_else(|| {
            AcquisitionError::clean(Failure::new(
                FailureCode::InvariantBreach,
                "validated process root is unavailable",
            ))
        })?;
        let cwd = open_beneath(root, &source.cwd_relative_path.0, true)
            .map_err(AcquisitionError::clean)?;

        let executable = os_string(&source.executable.0).map_err(AcquisitionError::clean)?;
        let arguments = source
            .argv
            .iter()
            .map(|argument| os_string(&argument.0))
            .collect::<Result<Vec<_>, _>>()
            .map_err(AcquisitionError::clean)?;
        let empty_environment = BTreeMap::new();
        let environment = match source.environment_profile.as_deref() {
            Some(id) => self.environment_profiles.get(id).ok_or_else(|| {
                AcquisitionError::clean(Failure::new(
                    FailureCode::InvariantBreach,
                    "validated environment profile is unavailable",
                ))
            })?,
            None => &empty_environment,
        };

        let mut command = Command::new(executable);
        command
            .args(arguments)
            .env_clear()
            .envs(environment)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let cwd_descriptor = cwd.as_raw_fd();
        // SAFETY: fchdir is async-signal-safe, the descriptor remains live
        // through spawn, and it names the directory opened beneath the
        // configured root without following symlinks.
        unsafe {
            command.pre_exec(move || {
                if libc::fchdir(cwd_descriptor) == 0 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            });
        }
        command.process_group(0);
        let started = Instant::now();
        let child = command.spawn().map_err(|_| {
            AcquisitionError::clean(Failure::new(
                FailureCode::AcquisitionFailed,
                "process spawn failed",
            ))
        })?;
        let deadline = started + Duration::from_millis(source.timeout_ms);
        let capture = process::capture(child, deadline)?;
        let process::ProcessCapture {
            bytes,
            events,
            terminal,
        } = capture;
        let (acquisition, terminal_failure) = terminal.into_acquisition(events);
        let source_bytes = bytes.len() as u64;
        let receipt = ValidatedAcquisition::process(
            &source.cwd_root_id,
            &source.cwd_relative_path,
            acquisition,
            source_bytes,
        );
        let acquired = Acquired::checked(bytes, receipt)?;
        if let Some(failure) = terminal_failure {
            return Err(AcquisitionError {
                failure,
                partial: Some(acquired),
            });
        }
        Ok(acquired)
    }
}

impl LocalRuntime for ProductionRuntime {
    fn acquire(&self, source: &LocalSource) -> Result<Acquired, AcquisitionError> {
        match source {
            LocalSource::Inline { bytes } => Self::acquire_inline(bytes),
            LocalSource::File {
                root_id,
                relative_path,
                reject_binary,
            } => self.acquire_file(root_id, relative_path, *reject_binary),
            LocalSource::Process(source) => self.acquire_process(source),
        }
    }

    fn now(&self) -> Result<u64, Failure> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .map_err(|_| {
                Failure::new(
                    FailureCode::InvariantBreach,
                    "system clock precedes the Unix epoch",
                )
            })
    }

    fn random_id(&self) -> Result<String, Failure> {
        let mut bytes = [0_u8; 16];
        File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut bytes))
            .map_err(|_| {
                Failure::new(
                    FailureCode::InvariantBreach,
                    "secure local randomness is unavailable",
                )
            })?;
        Ok(hex(&bytes))
    }
}

#[cfg(test)]
impl ProductionRuntime {
    fn acquire_source(&self, source: &Source) -> Result<Acquired, AcquisitionError> {
        let request = crate::types::Request {
            contract_version: CONTRACT_VERSION.to_owned(),
            request_id: "runtime-test".to_owned(),
            source: source.clone(),
            budget: Budget {
                unit: CountUnit::Bytes,
                total_visible_limit: 1,
                reserved_envelope: 0,
                token_profile: None,
            },
            preservation_profile: "plain-text/v1".to_owned(),
            retention: Retention::default(),
        };
        let prepared = request_policy::prepare(request, &self.request_config)
            .map_err(AcquisitionError::clean)?;
        let request_policy::ValidatedSource::Local(local) = prepared.source else {
            return Err(AcquisitionError::clean(Failure::new(
                FailureCode::SourceUnsupported,
                "artifact acquisition belongs to the artifact store",
            )));
        };
        <Self as LocalRuntime>::acquire(self, &local)
    }
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) struct FixtureRuntime {
    now: u64,
    next_id: AtomicU64,
}

#[cfg(test)]
impl FixtureRuntime {
    pub(crate) const fn new(now: u64) -> Self {
        Self {
            now,
            next_id: AtomicU64::new(1),
        }
    }
}

#[cfg(test)]
impl LocalRuntime for FixtureRuntime {
    fn acquire(&self, source: &LocalSource) -> Result<Acquired, AcquisitionError> {
        match source {
            LocalSource::Inline { bytes } => ProductionRuntime::acquire_inline(bytes),
            _ => Err(AcquisitionError::clean(Failure::new(
                FailureCode::SourceUnsupported,
                "deterministic fixture supports inline sources only",
            ))),
        }
    }

    fn now(&self) -> Result<u64, Failure> {
        Ok(self.now)
    }

    fn random_id(&self) -> Result<String, Failure> {
        Ok(format!(
            "{:032x}",
            self.next_id.fetch_add(1, Ordering::Relaxed)
        ))
    }
}

pub(crate) fn failure_receipt(source: &LocalSource) -> Result<ValidatedAcquisition, Failure> {
    let receipt = match source {
        LocalSource::Inline { .. } => Ok(ValidatedAcquisition::inline_failed()),
        LocalSource::File {
            root_id,
            relative_path,
            ..
        } => ValidatedAcquisition::file_failed(root_id, relative_path),
        LocalSource::Process(source) => ValidatedAcquisition::process(
            &source.cwd_root_id,
            &source.cwd_relative_path,
            ProcessAcquisition::Failed,
            0,
        ),
    };
    receipt.map_err(|message| Failure::new(FailureCode::InvariantBreach, message))
}

fn validate_root(path: &Path) -> Result<ConfiguredRoot, Failure> {
    if !path.is_absolute() {
        return Err(Failure::new(
            FailureCode::UnsafeRoot,
            "configured roots must be absolute",
        ));
    }
    verify_no_symlink_components(path)?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| Failure::new(FailureCode::UnsafeRoot, "configured root is unavailable"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Failure::new(
            FailureCode::UnsafeRoot,
            "configured root must be a real directory",
        ));
    }
    // SAFETY: geteuid has no preconditions and does not dereference memory.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(Failure::new(
            FailureCode::UnsafeRoot,
            "configured root is not owned by the current user",
        ));
    }
    Ok(ConfiguredRoot {
        path: path.to_path_buf(),
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn verify_no_symlink_components(path: &Path) -> Result<(), Failure> {
    let mut current = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir => continue,
            Component::Normal(value) => current.push(value),
            _ => {
                return Err(Failure::new(
                    FailureCode::UnsafeRoot,
                    "configured root contains an unsafe component",
                ));
            }
        }
        if fs::symlink_metadata(&current)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(Failure::new(
                FailureCode::UnsafeRoot,
                "configured root traverses a symlink",
            ));
        }
    }
    Ok(())
}

fn open_beneath(
    root: &ConfiguredRoot,
    relative: &[u8],
    require_directory: bool,
) -> Result<File, Failure> {
    let path = Path::new(OsStr::from_bytes(relative));
    let components = path
        .components()
        .map(|component| match component {
            Component::Normal(value) if !value.is_empty() => Ok(value),
            _ => Err(Failure::new(
                FailureCode::UnsafeRoot,
                "relative path contains an unsafe component",
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut current = File::open(&root.path).map_err(|_| {
        Failure::new(
            FailureCode::PermissionDenied,
            "configured root cannot be opened",
        )
    })?;
    let opened_root = current.metadata().map_err(|_| {
        Failure::new(
            FailureCode::UnsafeRoot,
            "configured root cannot be revalidated",
        )
    })?;
    if opened_root.dev() != root.device || opened_root.ino() != root.inode {
        return Err(Failure::new(
            FailureCode::UnsafeRoot,
            "configured root identity changed",
        ));
    }
    if components.is_empty() {
        if require_directory {
            return Ok(current);
        }
        return Err(Failure::new(
            FailureCode::UnsafeRoot,
            "file path must not be empty",
        ));
    }
    for (index, component) in components.iter().enumerate() {
        let final_component = index + 1 == components.len();
        let directory = !final_component || require_directory;
        let next = open_at(current.as_raw_fd(), component, directory)?;
        current = File::from(next);
    }
    Ok(current)
}

fn open_at(directory: i32, component: &OsStr, require_directory: bool) -> Result<OwnedFd, Failure> {
    let name = CString::new(component.as_bytes()).map_err(|_| {
        Failure::new(
            FailureCode::UnsafeRoot,
            "relative path contains an embedded NUL",
        )
    })?;
    let mut flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    if require_directory {
        flags |= libc::O_DIRECTORY;
    }
    // SAFETY: directory is a live directory descriptor, name is NUL-terminated,
    // and the returned descriptor is immediately given unique ownership.
    let descriptor = unsafe { libc::openat(directory, name.as_ptr(), flags) };
    if descriptor < 0 {
        let error = io::Error::last_os_error();
        return Err(match error.raw_os_error() {
            Some(libc::ELOOP | libc::ENOTDIR) => Failure::new(
                FailureCode::UnsafeRoot,
                "relative path traverses a symlink or non-directory",
            ),
            Some(libc::EACCES | libc::EPERM) => Failure::new(
                FailureCode::PermissionDenied,
                "relative path is not readable",
            ),
            _ => Failure::new(
                FailureCode::AcquisitionFailed,
                "relative path cannot be opened",
            ),
        });
    }
    // SAFETY: openat returned a new nonnegative descriptor owned by this call.
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
}

fn os_string(bytes: &[u8]) -> Result<OsString, Failure> {
    if bytes.contains(&0) {
        return Err(Failure::new(
            FailureCode::InvalidRequest,
            "process argv contains an embedded NUL",
        ));
    }
    Ok(OsString::from_vec(bytes.to_vec()))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        value.push(char::from(DIGITS[(byte >> 4) as usize]));
        value.push(char::from(DIGITS[(byte & 0x0f) as usize]));
    }
    value
}

#[cfg(test)]
#[path = "runtime/tests.rs"]
mod tests;
