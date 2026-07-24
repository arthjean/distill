use crate::types::{
    AcquisitionReceipt, ByteSpan, ByteString, EngineConfig, Failure, FailureCode, ProcessReceipt,
    ProcessStream, Source, SourceVariant, StreamEvent,
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
    process::{Child, Command, Stdio},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(crate) const MAX_SOURCE_BYTES: usize = 10 * 1024 * 1024;
const MAX_ARGUMENTS: usize = 4_096;
const MAX_ARGUMENT_BYTES: usize = 1024 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MIN_TIMEOUT_MS: u64 = 100;
const MAX_TIMEOUT_MS: u64 = 300_000;
const READ_CHUNK_BYTES: usize = 8 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct Acquired {
    pub bytes: Vec<u8>,
    pub receipt: AcquisitionReceipt,
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
    fn acquire(&self, source: &Source) -> Result<Acquired, AcquisitionError>;
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
            validate_identifier(id, "root ID")?;
            let root = validate_root(path)?;
            roots.insert(id.clone(), root);
        }
        for (id, profile) in &config.environment_profiles {
            validate_identifier(id, "environment profile ID")?;
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
        if bytes.0.len() > MAX_SOURCE_BYTES {
            return Err(AcquisitionError::clean(Failure::new(
                FailureCode::InputTooLarge,
                "inline source exceeds the 10 MiB limit",
            )));
        }
        Ok(Acquired {
            bytes: bytes.0.clone(),
            receipt: simple_receipt(SourceVariant::Inline),
        })
    }

    fn acquire_file(
        &self,
        root_id: &str,
        relative_path: &ByteString,
        reject_binary: bool,
    ) -> Result<Acquired, AcquisitionError> {
        let root = self.roots.get(root_id).ok_or_else(|| {
            AcquisitionError::clean(Failure::new(
                FailureCode::UnsafeRoot,
                "file source names an unknown configured root",
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
        Ok(Acquired {
            bytes,
            receipt: AcquisitionReceipt {
                variant: SourceVariant::File,
                complete: true,
                partial: false,
                truncated: false,
                root_id: Some(root_id.to_owned()),
                relative_path: Some(format!("<{} path bytes>", relative_path.0.len())),
                process: None,
            },
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn acquire_process(
        &self,
        executable: &ByteString,
        argv: &[ByteString],
        cwd_root_id: &str,
        cwd_relative_path: &ByteString,
        timeout_ms: Option<u64>,
        environment_profile: Option<&str>,
    ) -> Result<Acquired, AcquisitionError> {
        let timeout_ms = timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
            return Err(AcquisitionError::clean(Failure::new(
                FailureCode::InvalidRequest,
                "process timeout must be from 100 ms through 300 seconds",
            )));
        }
        let argument_bytes = argv.iter().try_fold(executable.0.len(), |total, argument| {
            total.checked_add(argument.0.len())
        });
        if argv.len() > MAX_ARGUMENTS
            || argument_bytes.is_none_or(|total| total > MAX_ARGUMENT_BYTES)
        {
            return Err(AcquisitionError::clean(Failure::new(
                FailureCode::ResourceExhausted,
                "process argv exceeds the configured limits",
            )));
        }
        let root = self.roots.get(cwd_root_id).ok_or_else(|| {
            AcquisitionError::clean(Failure::new(
                FailureCode::UnsafeRoot,
                "process source names an unknown configured root",
            ))
        })?;
        let cwd =
            open_beneath(root, &cwd_relative_path.0, true).map_err(AcquisitionError::clean)?;
        let cwd_path = descriptor_path(cwd.as_raw_fd());

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
                    FailureCode::InvalidRequest,
                    "process source names an unknown environment profile",
                ))
            })?,
            None => &empty_environment,
        };

        let mut command = Command::new(executable);
        command
            .args(arguments)
            .current_dir(cwd_path)
            .env_clear()
            .envs(environment)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.process_group(0);
        let mut child = command.spawn().map_err(|_| {
            AcquisitionError::clean(Failure::new(
                FailureCode::AcquisitionFailed,
                "process spawn failed",
            ))
        })?;
        let process_group = child.id();
        let stdout = child.stdout.take().ok_or_else(|| {
            AcquisitionError::clean(Failure::new(
                FailureCode::InvariantBreach,
                "process stdout pipe was not created",
            ))
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            AcquisitionError::clean(Failure::new(
                FailureCode::InvariantBreach,
                "process stderr pipe was not created",
            ))
        })?;

        let (sender, receiver) = mpsc::sync_channel(8);
        let stdout_thread = spawn_reader(stdout, ProcessStream::Stdout, sender.clone());
        let stderr_thread = spawn_reader(stderr, ProcessStream::Stderr, sender);
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut bytes = Vec::new();
        let mut events = Vec::new();
        let mut readers_done = 0_u8;
        let mut terminal_failure = None;
        let mut exit_status = None;
        let mut timed_out = false;

        while terminal_failure.is_none() && (readers_done < 2 || exit_status.is_none()) {
            let now = Instant::now();
            if now >= deadline {
                timed_out = true;
                terminal_failure = Some(Failure::new(
                    FailureCode::AcquisitionFailed,
                    "process exceeded its wall timeout",
                ));
                break;
            }
            if exit_status.is_none() {
                match child.try_wait() {
                    Ok(status) => exit_status = status,
                    Err(_) => {
                        terminal_failure = Some(Failure::new(
                            FailureCode::AcquisitionFailed,
                            "process status could not be inspected",
                        ));
                        break;
                    }
                }
            }
            if readers_done >= 2 {
                thread::park_timeout(
                    deadline
                        .saturating_duration_since(now)
                        .min(Duration::from_millis(10)),
                );
                continue;
            }
            let wait = deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(20));
            match receiver.recv_timeout(wait) {
                Ok(ReaderMessage::Chunk(stream, chunk)) => {
                    let remaining = MAX_SOURCE_BYTES.saturating_sub(bytes.len());
                    let accepted = chunk.len().min(remaining);
                    if accepted > 0 {
                        let start = bytes.len();
                        bytes.extend_from_slice(&chunk[..accepted]);
                        events.push(StreamEvent {
                            order: events.len() as u64,
                            stream,
                            span: ByteSpan {
                                start: start as u64,
                                end: bytes.len() as u64,
                            },
                        });
                    }
                    if accepted < chunk.len() {
                        terminal_failure = Some(Failure::new(
                            FailureCode::ResourceExhausted,
                            "process output exceeded the 10 MiB limit",
                        ));
                    }
                }
                Ok(ReaderMessage::Done) => readers_done += 1,
                Ok(ReaderMessage::Failed) => {
                    terminal_failure = Some(Failure::new(
                        FailureCode::AcquisitionFailed,
                        "process output capture failed",
                    ));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    terminal_failure = Some(Failure::new(
                        FailureCode::AcquisitionFailed,
                        "process output capture disconnected",
                    ));
                }
            }
        }

        if terminal_failure.is_some() {
            terminate_process_group(&mut child, process_group);
        }
        let status = match exit_status {
            Some(status) => status,
            None => child.wait().map_err(|_| {
                AcquisitionError::clean(Failure::new(
                    FailureCode::AcquisitionFailed,
                    "process wait failed",
                ))
            })?,
        };
        drop(receiver);
        let _stdout_joined = stdout_thread.join();
        let _stderr_joined = stderr_thread.join();
        let signal = process_signal(&status);
        if terminal_failure.is_none() && signal.is_some() {
            terminal_failure = Some(Failure::new(
                FailureCode::AcquisitionFailed,
                "process terminated by signal",
            ));
        }
        let partial = terminal_failure.is_some();
        let acquired = Acquired {
            bytes,
            receipt: AcquisitionReceipt {
                variant: SourceVariant::Process,
                complete: !partial,
                partial,
                truncated: terminal_failure
                    .as_ref()
                    .is_some_and(|failure| failure.code == FailureCode::ResourceExhausted),
                root_id: Some(cwd_root_id.to_owned()),
                relative_path: None,
                process: Some(ProcessReceipt {
                    events,
                    exit_code: status.code(),
                    signal,
                    timed_out,
                    working_directory: format!(
                        "{cwd_root_id}:<{} path bytes>",
                        cwd_relative_path.0.len()
                    ),
                }),
            },
        };
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
    fn acquire(&self, source: &Source) -> Result<Acquired, AcquisitionError> {
        match source {
            Source::Inline { bytes, .. } => Self::acquire_inline(bytes),
            Source::File {
                root_id,
                relative_path,
                binary_policy,
            } => self.acquire_file(
                root_id,
                relative_path,
                *binary_policy == crate::types::BinaryPolicy::Reject,
            ),
            Source::Process {
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
            Source::Artifact { .. } => Err(AcquisitionError::clean(Failure::new(
                FailureCode::SourceUnsupported,
                "artifact acquisition belongs to the artifact store",
            ))),
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
    fn acquire(&self, source: &Source) -> Result<Acquired, AcquisitionError> {
        match source {
            Source::Inline { bytes, .. } => ProductionRuntime::acquire_inline(bytes),
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

pub(crate) fn failure_receipt(source: &Source) -> AcquisitionReceipt {
    match source {
        Source::Inline { .. } => AcquisitionReceipt {
            complete: false,
            ..simple_receipt(SourceVariant::Inline)
        },
        Source::File {
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
        Source::Process {
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
        Source::Artifact { .. } => AcquisitionReceipt {
            complete: false,
            ..simple_receipt(SourceVariant::Artifact)
        },
    }
}

fn validate_identifier(value: &str, label: &str) -> Result<(), Failure> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(Failure::new(
            FailureCode::InvalidRequest,
            format!("{label} is invalid"),
        ));
    }
    Ok(())
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

fn descriptor_path(descriptor: i32) -> PathBuf {
    if cfg!(target_os = "linux") {
        PathBuf::from(format!("/proc/self/fd/{descriptor}"))
    } else {
        PathBuf::from(format!("/dev/fd/{descriptor}"))
    }
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

enum ReaderMessage {
    Chunk(ProcessStream, Vec<u8>),
    Done,
    Failed,
}

fn spawn_reader<R>(
    mut reader: R,
    stream: ProcessStream,
    sender: mpsc::SyncSender<ReaderMessage>,
) -> thread::JoinHandle<()>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buffer = [0_u8; READ_CHUNK_BYTES];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    let _sent = sender.send(ReaderMessage::Done);
                    return;
                }
                Ok(count) => {
                    if sender
                        .send(ReaderMessage::Chunk(stream, buffer[..count].to_vec()))
                        .is_err()
                    {
                        return;
                    }
                }
                Err(_) => {
                    let _sent = sender.send(ReaderMessage::Failed);
                    return;
                }
            }
        }
    })
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
        let process = acquired.receipt.process.expect("process receipt");
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
        assert!(partial.receipt.process.expect("process").timed_out);

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
