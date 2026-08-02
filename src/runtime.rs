#[cfg(test)]
use crate::request_policy;
#[cfg(test)]
use crate::types::Source;
use crate::{
    request_policy::{LocalSource, MAX_SOURCE_BYTES},
    types::{
        AcquisitionReceipt, ByteSpan, ByteString, EngineConfig, Failure, FailureCode,
        ProcessReceipt, ProcessStream, SourceVariant, StreamEvent, ValidatedAcquisition,
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
    process::{Child, ChildStderr, ChildStdout, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const READ_CHUNK_BYTES: usize = 8 * 1024;
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const PROCESS_REAP_TOLERANCE: Duration = Duration::from_millis(250);

#[derive(Clone, Debug)]
pub(crate) struct Acquired {
    pub bytes: Vec<u8>,
    pub receipt: ValidatedAcquisition,
}

impl Acquired {
    fn validated(bytes: Vec<u8>, receipt: AcquisitionReceipt) -> Result<Self, AcquisitionError> {
        let receipt =
            ValidatedAcquisition::new(receipt, bytes.len() as u64).map_err(|message| {
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
        })
    }

    fn acquire_inline(bytes: &ByteString) -> Result<Acquired, AcquisitionError> {
        Acquired::validated(bytes.0.clone(), simple_receipt(SourceVariant::Inline))
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
        Acquired::validated(
            bytes,
            AcquisitionReceipt {
                variant: SourceVariant::File,
                complete: true,
                partial: false,
                truncated: false,
                root_id: Some(root_id.to_owned()),
                relative_path: Some(format!("<{} path bytes>", relative_path.0.len())),
                process: None,
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn acquire_process(
        &self,
        executable: &ByteString,
        argv: &[ByteString],
        cwd_root_id: &str,
        cwd_relative_path: &ByteString,
        timeout_ms: u64,
        environment_profile: Option<&str>,
    ) -> Result<Acquired, AcquisitionError> {
        let root = self.roots.get(cwd_root_id).ok_or_else(|| {
            AcquisitionError::clean(Failure::new(
                FailureCode::InvariantBreach,
                "validated process root is unavailable",
            ))
        })?;
        let cwd =
            open_beneath(root, &cwd_relative_path.0, true).map_err(AcquisitionError::clean)?;

        let executable = os_string(&executable.0).map_err(AcquisitionError::clean)?;
        let arguments = argv
            .iter()
            .map(|argument| os_string(&argument.0))
            .collect::<Result<Vec<_>, _>>()
            .map_err(AcquisitionError::clean)?;
        let empty_environment = BTreeMap::new();
        let environment = match environment_profile {
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
        let deadline = started + Duration::from_millis(timeout_ms);
        let capture = ProcessLifecycle::new(child, deadline)?.capture()?;
        let status = capture.status;
        let signal = process_signal(&status);
        let terminal_failure = capture.failure;
        let partial = terminal_failure.is_some();
        let acquired = Acquired::validated(
            capture.bytes,
            AcquisitionReceipt {
                variant: SourceVariant::Process,
                complete: !partial,
                partial,
                truncated: terminal_failure
                    .as_ref()
                    .is_some_and(|failure| failure.code == FailureCode::ResourceExhausted),
                root_id: Some(cwd_root_id.to_owned()),
                relative_path: None,
                process: Some(ProcessReceipt {
                    events: capture.events,
                    exit_code: status.code(),
                    signal,
                    timed_out: capture.timed_out,
                    working_directory: format!(
                        "{cwd_root_id}:<{} path bytes>",
                        cwd_relative_path.0.len()
                    ),
                }),
            },
        )?;
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
            LocalSource::Process {
                executable,
                argv,
                cwd_root_id,
                cwd_relative_path,
                timeout_ms,
                environment_profile,
            } => self.acquire_process(
                executable,
                argv,
                cwd_root_id,
                cwd_relative_path,
                *timeout_ms,
                environment_profile.as_deref(),
            ),
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
    fn acquire(&self, source: &Source) -> Result<Acquired, AcquisitionError> {
        let local = match source {
            Source::Inline { bytes, .. } => {
                if bytes.0.len() > MAX_SOURCE_BYTES {
                    return Err(AcquisitionError::clean(Failure::new(
                        FailureCode::InputTooLarge,
                        "inline source exceeds the 10 MiB limit",
                    )));
                }
                LocalSource::Inline {
                    bytes: bytes.clone(),
                }
            }
            Source::File {
                root_id,
                relative_path,
                binary_policy,
            } => LocalSource::File {
                root_id: root_id.clone(),
                relative_path: relative_path.clone(),
                reject_binary: *binary_policy == crate::types::BinaryPolicy::Reject,
            },
            Source::Process {
                executable,
                argv,
                cwd_root_id,
                cwd_relative_path,
                timeout_ms,
                environment_profile,
            } => LocalSource::Process {
                executable: executable.clone(),
                argv: argv.clone(),
                cwd_root_id: cwd_root_id.clone(),
                cwd_relative_path: cwd_relative_path.clone(),
                timeout_ms: request_policy::validate_process(executable, argv, *timeout_ms)
                    .map_err(AcquisitionError::clean)?,
                environment_profile: environment_profile.clone(),
            },
            Source::Artifact { .. } => {
                return Err(AcquisitionError::clean(Failure::new(
                    FailureCode::SourceUnsupported,
                    "artifact acquisition belongs to the artifact store",
                )));
            }
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

fn simple_receipt(variant: SourceVariant) -> AcquisitionReceipt {
    AcquisitionReceipt {
        variant,
        complete: true,
        partial: false,
        truncated: false,
        root_id: None,
        relative_path: None,
        process: None,
    }
}

pub(crate) fn failure_receipt(source: &LocalSource) -> Result<ValidatedAcquisition, Failure> {
    let receipt = match source {
        LocalSource::Inline { .. } => AcquisitionReceipt {
            complete: false,
            ..simple_receipt(SourceVariant::Inline)
        },
        LocalSource::File {
            root_id,
            relative_path,
            ..
        } => AcquisitionReceipt {
            variant: SourceVariant::File,
            complete: false,
            partial: false,
            truncated: false,
            root_id: Some(root_id.clone()),
            relative_path: Some(format!("<{} path bytes>", relative_path.0.len())),
            process: None,
        },
        LocalSource::Process {
            cwd_root_id,
            cwd_relative_path,
            ..
        } => AcquisitionReceipt {
            variant: SourceVariant::Process,
            complete: false,
            partial: false,
            truncated: false,
            root_id: Some(cwd_root_id.clone()),
            relative_path: None,
            process: Some(ProcessReceipt {
                events: Vec::new(),
                exit_code: None,
                signal: None,
                timed_out: false,
                working_directory: format!(
                    "{cwd_root_id}:<{} path bytes>",
                    cwd_relative_path.0.len()
                ),
            }),
        },
    };
    ValidatedAcquisition::new(receipt, 0)
        .map_err(|message| Failure::new(FailureCode::InvariantBreach, message))
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

struct ProcessCapture {
    bytes: Vec<u8>,
    events: Vec<StreamEvent>,
    status: std::process::ExitStatus,
    failure: Option<Failure>,
    timed_out: bool,
}

struct ProcessLifecycle {
    child: Child,
    process_group: u32,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    timeout_at: Instant,
    deadline: Instant,
}

impl ProcessLifecycle {
    fn new(mut child: Child, timeout_at: Instant) -> Result<Self, AcquisitionError> {
        let process_group = child.id();
        let deadline = timeout_at + PROCESS_REAP_TOLERANCE;
        let setup = (|| {
            let stdout = child.stdout.take().ok_or_else(|| {
                Failure::new(
                    FailureCode::InvariantBreach,
                    "process stdout pipe was not created",
                )
            })?;
            let stderr = child.stderr.take().ok_or_else(|| {
                Failure::new(
                    FailureCode::InvariantBreach,
                    "process stderr pipe was not created",
                )
            })?;
            set_nonblocking(stdout.as_raw_fd())?;
            set_nonblocking(stderr.as_raw_fd())?;
            Ok((stdout, stderr))
        })();
        let (stdout, stderr) = match setup {
            Ok(pipes) => pipes,
            Err(failure) => {
                terminate_process_group(&mut child, process_group);
                if reap_child_until(&mut child, deadline).is_err() {
                    return Err(AcquisitionError::clean(Failure::new(
                        FailureCode::InvariantBreach,
                        "spawned process could not be reaped after capture setup failed",
                    )));
                }
                return Err(AcquisitionError::clean(failure));
            }
        };
        Ok(Self {
            child,
            process_group,
            stdout: Some(stdout),
            stderr: Some(stderr),
            timeout_at,
            deadline,
        })
    }

    fn capture(mut self) -> Result<ProcessCapture, AcquisitionError> {
        let mut bytes = Vec::new();
        let mut events = Vec::new();
        let mut status = None;
        let mut failure = None;
        let mut timed_out = false;

        while failure.is_none()
            && (status.is_none() || self.stdout.is_some() || self.stderr.is_some())
        {
            if Instant::now() >= self.timeout_at {
                timed_out = true;
                failure = Some(Failure::new(
                    FailureCode::AcquisitionFailed,
                    "process exceeded its wall timeout",
                ));
                break;
            }
            if let Some(capture_failure) = self.drain_ready_streams(&mut bytes, &mut events) {
                failure = Some(capture_failure);
                break;
            }
            if status.is_none() {
                match self.child.try_wait() {
                    Ok(child_status) => {
                        status = child_status;
                    }
                    Err(_) => {
                        failure = Some(Failure::new(
                            FailureCode::AcquisitionFailed,
                            "process status could not be inspected",
                        ));
                        break;
                    }
                }
            }
            if status.is_some() && self.stdout.is_none() && self.stderr.is_none() {
                break;
            }
            if let Err(poll_failure) =
                poll_streams(self.stdout.as_ref(), self.stderr.as_ref(), self.deadline)
            {
                failure = Some(poll_failure);
                break;
            }
        }

        if failure.is_some() {
            // Dropping both read ends releases reader completion even when a
            // descendant escaped the owned process group with inherited writes.
            self.stdout.take();
            self.stderr.take();
            terminate_process_group(&mut self.child, self.process_group);
        }
        let status = match status {
            Some(status) => status,
            None => {
                reap_child_until(&mut self.child, self.deadline).map_err(AcquisitionError::clean)?
            }
        };
        let signal = process_signal(&status);
        if failure.is_none() && signal.is_some() {
            failure = Some(Failure::new(
                FailureCode::AcquisitionFailed,
                "process terminated by signal",
            ));
        }
        Ok(ProcessCapture {
            bytes,
            events,
            status,
            failure,
            timed_out,
        })
    }

    fn drain_ready_streams(
        &mut self,
        bytes: &mut Vec<u8>,
        events: &mut Vec<StreamEvent>,
    ) -> Option<Failure> {
        if let Some(failure) = read_stream(&mut self.stdout, ProcessStream::Stdout, bytes, events) {
            return Some(failure);
        }
        read_stream(&mut self.stderr, ProcessStream::Stderr, bytes, events)
    }
}

fn read_stream<R>(
    reader: &mut Option<R>,
    stream: ProcessStream,
    bytes: &mut Vec<u8>,
    events: &mut Vec<StreamEvent>,
) -> Option<Failure>
where
    R: Read,
{
    let pipe = reader.as_mut()?;
    let mut buffer = [0_u8; READ_CHUNK_BYTES];
    match pipe.read(&mut buffer) {
        Ok(0) => {
            *reader = None;
            None
        }
        Ok(count) => {
            let remaining = MAX_SOURCE_BYTES.saturating_sub(bytes.len());
            let accepted = count.min(remaining);
            if accepted > 0 {
                let start = bytes.len();
                bytes.extend_from_slice(&buffer[..accepted]);
                events.push(StreamEvent {
                    order: events.len() as u64,
                    stream,
                    span: ByteSpan {
                        start: start as u64,
                        end: bytes.len() as u64,
                    },
                });
            }
            if accepted < count {
                Some(Failure::new(
                    FailureCode::ResourceExhausted,
                    "process output exceeded the 10 MiB limit",
                ))
            } else {
                None
            }
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => None,
        Err(error) if error.kind() == io::ErrorKind::Interrupted => None,
        Err(_) => Some(Failure::new(
            FailureCode::AcquisitionFailed,
            "process output capture failed",
        )),
    }
}

fn set_nonblocking(descriptor: libc::c_int) -> Result<(), Failure> {
    // SAFETY: fcntl reads flags from the live child pipe descriptor.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 {
        return Err(Failure::new(
            FailureCode::AcquisitionFailed,
            "nonblocking process capture is unavailable",
        ));
    }
    // SAFETY: the same live descriptor remains owned by ChildStdout or
    // ChildStderr, and O_NONBLOCK changes only its file status flags.
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(Failure::new(
            FailureCode::AcquisitionFailed,
            "nonblocking process capture is unavailable",
        ));
    }
    Ok(())
}

fn poll_streams(
    stdout: Option<&ChildStdout>,
    stderr: Option<&ChildStderr>,
    deadline: Instant,
) -> Result<(), Failure> {
    let mut descriptors = [
        libc::pollfd {
            fd: stdout.map_or(-1, AsRawFd::as_raw_fd),
            events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
            revents: 0,
        },
        libc::pollfd {
            fd: stderr.map_or(-1, AsRawFd::as_raw_fd),
            events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
            revents: 0,
        },
    ];
    let timeout = poll_timeout(deadline);
    // SAFETY: descriptors points to two initialized pollfd values for the
    // duration of poll, and negative descriptors are ignored by POSIX poll.
    let result = unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, timeout) };
    if result >= 0 || io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
        Ok(())
    } else {
        Err(Failure::new(
            FailureCode::AcquisitionFailed,
            "process output readiness could not be inspected",
        ))
    }
}

fn poll_timeout(deadline: Instant) -> libc::c_int {
    let wait = deadline
        .saturating_duration_since(Instant::now())
        .min(PROCESS_POLL_INTERVAL);
    if wait.is_zero() {
        return 0;
    }
    let milliseconds = wait.as_millis().max(1);
    libc::c_int::try_from(milliseconds).unwrap_or(libc::c_int::MAX)
}

fn reap_child_until(
    child: &mut Child,
    deadline: Instant,
) -> Result<std::process::ExitStatus, Failure> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => {
                poll_streams(None, None, deadline)?;
            }
            Ok(None) => {
                return Err(Failure::new(
                    FailureCode::InvariantBreach,
                    "process could not be reaped before its lifecycle deadline",
                ));
            }
            Err(_) => {
                return Err(Failure::new(
                    FailureCode::AcquisitionFailed,
                    "process wait failed",
                ));
            }
        }
    }
}

fn terminate_process_group(child: &mut Child, process_group: u32) {
    if let Ok(group) = i32::try_from(process_group) {
        // SAFETY: the child starts in a fresh process group. Negating its PID
        // targets only that acquisition tree, never the Distill process group.
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
    }
    let _killed = child.kill();
}

fn process_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
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
        let acquired = runtime.acquire(&source).expect("inline");
        assert_eq!(acquired.bytes, [0, 0xff, b'x']);

        let oversized = Source::Inline {
            bytes: ByteString(vec![0; MAX_SOURCE_BYTES + 1]),
            media_type: None,
        };
        assert_eq!(
            runtime
                .acquire(&oversized)
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
        let acquired = runtime.acquire(&source).expect("file");
        assert_eq!(acquired.bytes, [0, 0xff, b'x']);
        assert_eq!(acquired.receipt.root_id.as_deref(), Some("workspace"));
        assert!(
            !acquired
                .receipt
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
            runtime.acquire(&rejected).expect_err("binary").failure.code,
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
                .acquire(&unknown)
                .expect_err("unknown root")
                .failure
                .code,
            FailureCode::InvariantBreach
        );

        fs::create_dir(directory.path().join("directory")).expect("directory");
        let non_regular = Source::File {
            root_id: "workspace".to_owned(),
            relative_path: ByteString::from_utf8("directory"),
            binary_policy: BinaryPolicy::Accept,
        };
        assert_eq!(
            runtime
                .acquire(&non_regular)
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
                .acquire(&oversized_source)
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
                .acquire(&invalid_utf8)
                .expect_err("invalid UTF-8")
                .failure
                .code,
            FailureCode::AcquisitionFailed
        );

        assert_eq!(
            runtime
                .acquire(&Source::Artifact {
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
            let code = runtime.acquire(&source).expect_err("unsafe").failure.code;
            assert!(
                matches!(
                    code,
                    FailureCode::UnsafeRoot | FailureCode::AcquisitionFailed
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
            runtime.acquire(&source).expect_err("swapped").failure.code,
            FailureCode::UnsafeRoot
        );

        fs::remove_file(allowed.join("nested")).expect("remove symlink");
        fs::rename(allowed.join("nested-original"), allowed.join("nested")).expect("restore");
        assert_eq!(runtime.acquire(&source).expect("safe").bytes, b"safe");

        fs::rename(&allowed, parent.path().join("allowed-original")).expect("move root");
        symlink(&outside, &allowed).expect("root swap");
        assert_eq!(
            runtime
                .acquire(&source)
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
        let acquired = runtime.acquire(&source).expect("process");
        let text = String::from_utf8(acquired.bytes).expect("utf8");
        assert!(text.contains("--literal"));
        assert!(text.contains("value with spaces"));
        assert!(text.contains(&injection));
        assert!(!marker.exists());
        let process = acquired.receipt.process.as_ref().expect("process receipt");
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
        let timed_out = runtime.acquire(&timeout).expect_err("timeout");
        assert_eq!(timed_out.failure.code, FailureCode::AcquisitionFailed);
        let partial = timed_out.partial.expect("timeout receipt");
        assert!(partial.receipt.partial);
        assert!(partial.receipt.process.as_ref().expect("process").timed_out);

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
            .acquire(&closed_streams)
            .expect_err("closed streams still obey timeout");
        assert!(
            timed_out
                .partial
                .expect("closed stream receipt")
                .receipt
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
        let killed = runtime.acquire(&signaled).expect_err("signal");
        assert_eq!(killed.failure.code, FailureCode::AcquisitionFailed);
        assert!(
            killed
                .partial
                .expect("signal receipt")
                .receipt
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
        let exhausted = runtime.acquire(&chatter).expect_err("output cap");
        assert_eq!(exhausted.failure.code, FailureCode::ResourceExhausted);
        let partial = exhausted.partial.expect("partial output");
        assert_eq!(partial.bytes.len(), MAX_SOURCE_BYTES);
        assert!(partial.receipt.truncated);
        assert!(!partial.receipt.complete);
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
                .acquire(&invalid_timeout)
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
            runtime.acquire(&nul).expect_err("nul").failure.code,
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
                .acquire(&unknown_environment)
                .expect_err("environment")
                .failure
                .code,
            FailureCode::InvariantBreach
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
