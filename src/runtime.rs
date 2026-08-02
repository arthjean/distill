#[cfg(test)]
use crate::request_policy;
#[cfg(test)]
use crate::types::{Budget, CONTRACT_VERSION, CountUnit, Retention, Source};

mod process;
use crate::{
    request_policy::{LocalSource, MAX_SOURCE_BYTES, ProcessSource},
    types::{
        ByteString, EngineConfig, Failure, FailureCode, ProcessAcquisition, ProcessPartialReason,
        ProcessTermination, ValidatedAcquisition,
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
            status,
            failure: terminal_failure,
            timed_out,
        } = capture;
        let signal = process::signal(&status);
        let acquisition = if let Some(failure) = terminal_failure.as_ref() {
            let termination = match (status.code(), signal) {
                (Some(code), None) => ProcessTermination::Exit(code),
                (None, Some(signal)) => ProcessTermination::Signal(signal),
                _ => {
                    return Err(AcquisitionError::clean(Failure::new(
                        FailureCode::InvariantBreach,
                        "process capture returned an invalid terminal state",
                    )));
                }
            };
            let reason = if timed_out {
                ProcessPartialReason::TimedOut
            } else if failure.code == FailureCode::ResourceExhausted {
                ProcessPartialReason::Truncated
            } else {
                ProcessPartialReason::CaptureFailed
            };
            ProcessAcquisition::Partial {
                events,
                termination,
                reason,
            }
        } else {
            let exit_code = status.code().ok_or_else(|| {
                AcquisitionError::clean(Failure::new(
                    FailureCode::InvariantBreach,
                    "complete process capture did not return an exit code",
                ))
            })?;
            ProcessAcquisition::Complete { events, exit_code }
        };
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
mod tests {
    use super::*;
    use crate::types::{BinaryPolicy, EngineConfig};
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    fn runtime() -> (TempDir, ProductionRuntime) {
        let directory = tempfile::tempdir().expect("temp directory");
        let mut config = EngineConfig::local(directory.path().join("store/store.sqlite"));
        config
            .roots
            .insert("workspace".to_owned(), directory.path().to_path_buf());
        config.environment_profiles.insert(
            "safe".to_owned(),
            BTreeMap::from([("DISTILL_SAFE".to_owned(), "value".to_owned())]),
        );
        let runtime = ProductionRuntime::new(&config).expect("runtime");
        (directory, runtime)
    }

    #[test]
    fn production_runtime_rejects_each_invalid_configuration_shape() {
        let directory = tempfile::tempdir().expect("temp directory");
        let base = EngineConfig::local(directory.path().join("store.sqlite"));

        let mut invalid_root_id = base.clone();
        invalid_root_id
            .roots
            .insert(String::new(), directory.path().to_path_buf());
        assert_eq!(
            ProductionRuntime::new(&invalid_root_id)
                .expect_err("invalid root ID")
                .safe_message,
            "root ID is invalid"
        );

        let mut invalid_profile_id = base.clone();
        invalid_profile_id
            .environment_profiles
            .insert(String::new(), BTreeMap::new());
        assert_eq!(
            ProductionRuntime::new(&invalid_profile_id)
                .expect_err("invalid profile ID")
                .safe_message,
            "environment profile ID is invalid"
        );

        let too_many_variables = (0..129)
            .map(|index| (format!("VARIABLE_{index}"), String::new()))
            .collect();
        let invalid_variables = [
            too_many_variables,
            BTreeMap::from([(String::new(), String::new())]),
            BTreeMap::from([("K".repeat(129), String::new())]),
            BTreeMap::from([("VARIABLE".to_owned(), "v".repeat(4_097))]),
            BTreeMap::from([("INVALID=VARIABLE".to_owned(), String::new())]),
            BTreeMap::from([("INVALID\0VARIABLE".to_owned(), String::new())]),
            BTreeMap::from([("VARIABLE".to_owned(), "invalid\0value".to_owned())]),
        ];

        for variables in invalid_variables {
            let mut config = base.clone();
            config
                .environment_profiles
                .insert("profile".to_owned(), variables);
            assert_eq!(
                ProductionRuntime::new(&config)
                    .expect_err("invalid environment profile")
                    .code,
                FailureCode::InvalidRequest
            );
        }
    }

    #[test]
    fn inline_capture_preserves_arbitrary_bytes_and_enforces_limit() {
        let (_directory, runtime) = runtime();
        let source = Source::Inline {
            bytes: ByteString::from(&[0, 0xff, b'x'][..]),
            media_type: None,
        };
        let acquired = runtime.acquire_source(&source).expect("inline");
        assert_eq!(acquired.bytes, [0, 0xff, b'x']);

        let oversized = Source::Inline {
            bytes: ByteString(vec![0; MAX_SOURCE_BYTES + 1]),
            media_type: None,
        };
        assert_eq!(
            runtime
                .acquire_source(&oversized)
                .expect_err("oversized")
                .failure
                .code,
            FailureCode::InputTooLarge
        );
    }

    #[test]
    fn file_capture_is_descriptor_relative_and_binary_aware() {
        let (directory, runtime) = runtime();
        fs::create_dir(directory.path().join("nested")).expect("nested");
        fs::write(directory.path().join("nested/data.bin"), [0, 0xff, b'x']).expect("file");
        let source = Source::File {
            root_id: "workspace".to_owned(),
            relative_path: ByteString::from_utf8("nested/data.bin"),
            binary_policy: BinaryPolicy::Accept,
        };
        let acquired = runtime.acquire_source(&source).expect("file");
        assert_eq!(acquired.bytes, [0, 0xff, b'x']);
        assert_eq!(
            acquired.receipt.as_receipt().root_id.as_deref(),
            Some("workspace")
        );
        assert!(
            !acquired
                .receipt
                .as_receipt()
                .relative_path
                .as_ref()
                .expect("safe path")
                .contains("data.bin")
        );

        let mut rejected = source;
        if let Source::File { binary_policy, .. } = &mut rejected {
            *binary_policy = BinaryPolicy::Reject;
        }
        assert_eq!(
            runtime
                .acquire_source(&rejected)
                .expect_err("binary")
                .failure
                .code,
            FailureCode::AcquisitionFailed
        );
    }

    #[test]
    fn file_capture_rejects_unknown_non_regular_oversized_and_invalid_utf8_sources() {
        let (directory, runtime) = runtime();

        let unknown = Source::File {
            root_id: "unknown".to_owned(),
            relative_path: ByteString::from_utf8("data"),
            binary_policy: BinaryPolicy::Accept,
        };
        assert_eq!(
            runtime
                .acquire_source(&unknown)
                .expect_err("unknown root")
                .failure
                .code,
            FailureCode::UnsafeRoot
        );

        fs::create_dir(directory.path().join("directory")).expect("directory");
        let non_regular = Source::File {
            root_id: "workspace".to_owned(),
            relative_path: ByteString::from_utf8("directory"),
            binary_policy: BinaryPolicy::Accept,
        };
        assert_eq!(
            runtime
                .acquire_source(&non_regular)
                .expect_err("non-regular source")
                .failure
                .code,
            FailureCode::AcquisitionFailed
        );

        let oversized_path = directory.path().join("oversized");
        let oversized = File::create(&oversized_path).expect("oversized file");
        oversized
            .set_len(MAX_SOURCE_BYTES as u64 + 1)
            .expect("sparse oversized file");
        let oversized_source = Source::File {
            root_id: "workspace".to_owned(),
            relative_path: ByteString::from_utf8("oversized"),
            binary_policy: BinaryPolicy::Accept,
        };
        assert_eq!(
            runtime
                .acquire_source(&oversized_source)
                .expect_err("oversized source")
                .failure
                .code,
            FailureCode::InputTooLarge
        );

        fs::write(directory.path().join("invalid-utf8"), [0xff]).expect("invalid UTF-8");
        let invalid_utf8 = Source::File {
            root_id: "workspace".to_owned(),
            relative_path: ByteString::from_utf8("invalid-utf8"),
            binary_policy: BinaryPolicy::Reject,
        };
        assert_eq!(
            runtime
                .acquire_source(&invalid_utf8)
                .expect_err("invalid UTF-8")
                .failure
                .code,
            FailureCode::AcquisitionFailed
        );

        assert_eq!(
            runtime
                .acquire_source(&Source::Artifact {
                    artifact: crate::types::ArtifactRef {
                        schema_version: crate::types::ARTIFACT_SCHEMA_VERSION.to_owned(),
                        id: "a".repeat(32),
                        source_sha256: "b".repeat(64),
                        source_bytes: 0,
                        created_at: 1,
                        expires_at: 2,
                    },
                })
                .expect_err("artifact source")
                .failure
                .code,
            FailureCode::SourceUnsupported
        );
    }

    #[test]
    fn traversal_symlinks_and_embedded_nul_fail_closed() {
        let (directory, runtime) = runtime();
        fs::write(directory.path().join("safe.txt"), b"safe").expect("safe");
        symlink("/etc/passwd", directory.path().join("escape")).expect("symlink");
        for path in [
            ByteString::from_utf8("../outside"),
            ByteString::from_utf8("/absolute"),
            ByteString::from_utf8("escape"),
            ByteString(b"safe\0.txt".to_vec()),
        ] {
            let source = Source::File {
                root_id: "workspace".to_owned(),
                relative_path: path,
                binary_policy: BinaryPolicy::Accept,
            };
            let code = runtime
                .acquire_source(&source)
                .expect_err("unsafe")
                .failure
                .code;
            assert!(
                matches!(
                    code,
                    FailureCode::InvalidRequest
                        | FailureCode::UnsafeRoot
                        | FailureCode::AcquisitionFailed
                ),
                "unexpected code: {code:?}"
            );
        }
    }

    #[test]
    fn symlink_replacement_after_root_configuration_never_escapes() {
        let parent = tempfile::tempdir().expect("temp directory");
        let allowed = parent.path().join("allowed");
        let outside = parent.path().join("outside");
        fs::create_dir_all(allowed.join("nested")).expect("allowed");
        fs::create_dir_all(&outside).expect("outside");
        fs::write(allowed.join("nested/data"), b"safe").expect("safe");
        fs::write(outside.join("data"), b"secret").expect("secret");
        let mut config = EngineConfig::local(parent.path().join("store/store.sqlite"));
        config.roots.insert("workspace".to_owned(), allowed.clone());
        let runtime = ProductionRuntime::new(&config).expect("runtime");

        fs::rename(allowed.join("nested"), allowed.join("nested-original")).expect("move original");
        symlink(&outside, allowed.join("nested")).expect("swap symlink");
        let source = Source::File {
            root_id: "workspace".to_owned(),
            relative_path: ByteString::from_utf8("nested/data"),
            binary_policy: BinaryPolicy::Accept,
        };
        assert_eq!(
            runtime
                .acquire_source(&source)
                .expect_err("swapped")
                .failure
                .code,
            FailureCode::UnsafeRoot
        );

        fs::remove_file(allowed.join("nested")).expect("remove symlink");
        fs::rename(allowed.join("nested-original"), allowed.join("nested")).expect("restore");
        assert_eq!(
            runtime.acquire_source(&source).expect("safe").bytes,
            b"safe"
        );

        fs::rename(&allowed, parent.path().join("allowed-original")).expect("move root");
        symlink(&outside, &allowed).expect("root swap");
        assert_eq!(
            runtime
                .acquire_source(&source)
                .expect_err("root replaced")
                .failure
                .code,
            FailureCode::UnsafeRoot
        );
    }

    #[test]
    fn symlinked_or_foreign_root_configuration_is_rejected() {
        let directory = tempfile::tempdir().expect("temp directory");
        let actual = directory.path().join("actual");
        fs::create_dir(&actual).expect("actual");
        let linked = directory.path().join("linked");
        symlink(&actual, &linked).expect("linked");
        let mut config = EngineConfig::local(directory.path().join("store.sqlite"));
        config.roots.insert("root".to_owned(), linked);
        assert_eq!(
            ProductionRuntime::new(&config)
                .expect_err("symlink root")
                .code,
            FailureCode::UnsafeRoot
        );
    }

    #[test]
    fn argv_is_literal_environment_is_allowlisted_and_no_shell_is_inferred() {
        let (directory, runtime) = runtime();
        let marker = directory.path().join("must-not-exist");
        let injection = format!("$(touch {})", marker.display());
        let source = Source::Process {
            executable: ByteString::from_utf8(command_path(&["/usr/bin/printf", "/bin/printf"])),
            argv: vec![
                ByteString::from_utf8("%s\n"),
                ByteString::from_utf8("--literal"),
                ByteString::from_utf8("value with spaces"),
                ByteString::from_utf8(injection.clone()),
            ],
            cwd_root_id: "workspace".to_owned(),
            cwd_relative_path: ByteString::default(),
            timeout_ms: Some(2_000),
            environment_profile: Some("safe".to_owned()),
        };
        let acquired = runtime.acquire_source(&source).expect("process");
        let text = String::from_utf8(acquired.bytes).expect("utf8");
        assert!(text.contains("--literal"));
        assert!(text.contains("value with spaces"));
        assert!(text.contains(&injection));
        assert!(!marker.exists());
        let process = acquired
            .receipt
            .as_receipt()
            .process
            .as_ref()
            .expect("process receipt");
        assert_eq!(process.exit_code, Some(0));
        assert_eq!(process.signal, None);
        assert!(!process.timed_out);
        assert!(!process.events.is_empty());
    }

    #[test]
    fn timeout_signal_and_output_limit_return_partial_typed_failures() {
        let (_directory, runtime) = runtime();
        let timeout = Source::Process {
            executable: ByteString::from_utf8(command_path(&["/usr/bin/sleep", "/bin/sleep"])),
            argv: vec![ByteString::from_utf8("5")],
            cwd_root_id: "workspace".to_owned(),
            cwd_relative_path: ByteString::default(),
            timeout_ms: Some(100),
            environment_profile: None,
        };
        let timed_out = runtime.acquire_source(&timeout).expect_err("timeout");
        assert_eq!(timed_out.failure.code, FailureCode::AcquisitionFailed);
        let partial = timed_out.partial.expect("timeout receipt");
        assert!(partial.receipt.as_receipt().partial);
        assert!(
            partial
                .receipt
                .as_receipt()
                .process
                .as_ref()
                .expect("process")
                .timed_out
        );

        let closed_streams = Source::Process {
            executable: ByteString::from_utf8(command_path(&["/bin/sh", "/usr/bin/sh"])),
            argv: vec![
                ByteString::from_utf8("-c"),
                ByteString::from_utf8("exec 1>&- 2>&-; sleep 5"),
            ],
            cwd_root_id: "workspace".to_owned(),
            cwd_relative_path: ByteString::default(),
            timeout_ms: Some(100),
            environment_profile: None,
        };
        let timed_out = runtime
            .acquire_source(&closed_streams)
            .expect_err("closed streams still obey timeout");
        assert!(
            timed_out
                .partial
                .expect("closed stream receipt")
                .receipt
                .as_receipt()
                .process
                .as_ref()
                .expect("process")
                .timed_out
        );

        let signaled = Source::Process {
            executable: ByteString::from_utf8(command_path(&["/bin/sh", "/usr/bin/sh"])),
            argv: vec![
                ByteString::from_utf8("-c"),
                ByteString::from_utf8("kill -TERM $$"),
            ],
            cwd_root_id: "workspace".to_owned(),
            cwd_relative_path: ByteString::default(),
            timeout_ms: Some(2_000),
            environment_profile: None,
        };
        let killed = runtime.acquire_source(&signaled).expect_err("signal");
        assert_eq!(killed.failure.code, FailureCode::AcquisitionFailed);
        assert!(
            killed
                .partial
                .expect("signal receipt")
                .receipt
                .as_receipt()
                .process
                .as_ref()
                .expect("process")
                .signal
                .is_some()
        );

        let chatter = Source::Process {
            executable: ByteString::from_utf8(command_path(&["/usr/bin/yes", "/bin/yes"])),
            argv: Vec::new(),
            cwd_root_id: "workspace".to_owned(),
            cwd_relative_path: ByteString::default(),
            timeout_ms: Some(5_000),
            environment_profile: None,
        };
        let exhausted = runtime.acquire_source(&chatter).expect_err("output cap");
        assert_eq!(exhausted.failure.code, FailureCode::ResourceExhausted);
        let partial = exhausted.partial.expect("partial output");
        assert_eq!(partial.bytes.len(), MAX_SOURCE_BYTES);
        assert!(partial.receipt.as_receipt().truncated);
        assert!(!partial.receipt.as_receipt().complete);
    }

    #[test]
    fn process_request_validation_is_bounded_and_deterministic() {
        let (_directory, runtime) = runtime();
        let invalid_timeout = Source::Process {
            executable: ByteString::from_utf8("/usr/bin/true"),
            argv: Vec::new(),
            cwd_root_id: "workspace".to_owned(),
            cwd_relative_path: ByteString::default(),
            timeout_ms: Some(99),
            environment_profile: None,
        };
        assert_eq!(
            runtime
                .acquire_source(&invalid_timeout)
                .expect_err("timeout range")
                .failure
                .code,
            FailureCode::InvalidRequest
        );

        let nul = Source::Process {
            executable: ByteString(b"/bin/tr\0ue".to_vec()),
            argv: Vec::new(),
            cwd_root_id: "workspace".to_owned(),
            cwd_relative_path: ByteString::default(),
            timeout_ms: None,
            environment_profile: None,
        };
        assert_eq!(
            runtime.acquire_source(&nul).expect_err("nul").failure.code,
            FailureCode::InvalidRequest
        );

        let unknown_environment = Source::Process {
            executable: ByteString::from_utf8("/usr/bin/true"),
            argv: Vec::new(),
            cwd_root_id: "workspace".to_owned(),
            cwd_relative_path: ByteString::default(),
            timeout_ms: None,
            environment_profile: Some("unknown".to_owned()),
        };
        assert_eq!(
            runtime
                .acquire_source(&unknown_environment)
                .expect_err("environment")
                .failure
                .code,
            FailureCode::InvalidRequest
        );
    }

    #[test]
    fn production_ids_are_opaque_and_fixture_ids_are_stable() {
        let (_directory, runtime) = runtime();
        let first = runtime.random_id().expect("random ID");
        let second = runtime.random_id().expect("random ID");
        assert_eq!(first.len(), 32);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);

        let fixture = FixtureRuntime::new(42);
        assert_eq!(fixture.now().expect("clock"), 42);
        assert_eq!(
            fixture.random_id().expect("fixture ID"),
            "00000000000000000000000000000001"
        );
    }

    fn command_path(candidates: &[&str]) -> String {
        candidates
            .iter()
            .find(|path| Path::new(path).exists())
            .expect("required test command")
            .to_string()
    }
}
