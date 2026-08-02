use super::AcquisitionError;
use crate::{
    request_policy::MAX_SOURCE_BYTES,
    types::{ByteSpan, Failure, FailureCode, ProcessStream, StreamEvent},
};
use std::{
    io::{self, Read},
    os::fd::AsRawFd,
    process::{Child, ChildStderr, ChildStdout},
    time::{Duration, Instant},
};

const READ_CHUNK_BYTES: usize = 8 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const REAP_TOLERANCE: Duration = Duration::from_millis(250);

pub(super) struct ProcessCapture {
    pub bytes: Vec<u8>,
    pub events: Vec<StreamEvent>,
    pub status: std::process::ExitStatus,
    pub failure: Option<Failure>,
    pub timed_out: bool,
}

pub(super) fn capture(
    child: Child,
    timeout_at: Instant,
) -> Result<ProcessCapture, AcquisitionError> {
    ProcessLifecycle::new(child, timeout_at)?.capture()
}

pub(super) fn signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
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
        let deadline = timeout_at + REAP_TOLERANCE;
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
                    Ok(child_status) => status = child_status,
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
        if failure.is_none() && signal(&status).is_some() {
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
    // SAFETY: `descriptor` belongs to a live child-pipe handle for this call.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 {
        return Err(Failure::new(
            FailureCode::AcquisitionFailed,
            "nonblocking process capture is unavailable",
        ));
    }
    // SAFETY: `descriptor` is still live and the operation only updates its flags.
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
    // SAFETY: the slice supplies initialized `pollfd` values and remains alive for the call.
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
        .min(POLL_INTERVAL);
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
            Ok(None) if Instant::now() < deadline => poll_streams(None, None, deadline)?,
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
        // SAFETY: spawned acquisitions create a fresh process group whose id is the child pid.
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
    }
    let _killed = child.kill();
}
