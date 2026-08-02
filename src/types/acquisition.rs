use super::{AcquisitionReceipt, ByteString, ProcessReceipt, SourceVariant, StreamEvent};
#[derive(Clone, Debug)]
pub(crate) struct ValidatedAcquisition {
    state: AcquisitionState,
    source_bytes: u64,
}

#[derive(Clone, Debug)]
enum AcquisitionState {
    Inline(Completion),
    File {
        root_id: String,
        relative_path: PathSummary,
        completion: Completion,
    },
    Process {
        root_id: String,
        working_directory: PathSummary,
        state: ProcessState,
    },
    Artifact(Completion),
}

#[derive(Clone, Copy, Debug)]
enum Completion {
    Complete,
    Failed,
}

impl Completion {
    fn from_complete(complete: bool) -> Self {
        if complete {
            Self::Complete
        } else {
            Self::Failed
        }
    }

    fn is_complete(self) -> bool {
        matches!(self, Self::Complete)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ProcessTermination {
    Exit(i32),
    Signal(i32),
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ProcessPartialReason {
    TimedOut,
    Truncated,
    TimedOutAndTruncated,
    CaptureFailed,
}

#[derive(Clone, Debug)]
enum ProcessState {
    Failed,
    Complete {
        events: Vec<StreamEvent>,
        exit_code: i32,
    },
    Partial {
        events: Vec<StreamEvent>,
        termination: ProcessTermination,
        reason: ProcessPartialReason,
    },
}

#[derive(Clone, Debug)]
struct PathSummary(String);

impl PathSummary {
    fn from_bytes(bytes: &ByteString) -> Result<Self, &'static str> {
        let bytes = u64::try_from(bytes.0.len()).map_err(|_| "path summary is too large")?;
        if bytes > crate::contract::MAX_PATH_BYTES as u64 {
            return Err("path summary is too large");
        }
        Ok(Self(format!("<{bytes} path bytes>")))
    }

    fn parse(value: &str) -> Option<Self> {
        let value = value.strip_prefix('<')?.strip_suffix(" path bytes>")?;
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let bytes = value.parse::<u64>().ok()?;
        (bytes <= crate::contract::MAX_PATH_BYTES as u64)
            .then(|| Self(format!("<{value} path bytes>")))
    }

    fn render(&self) -> &str {
        &self.0
    }
}

impl ValidatedAcquisition {
    pub(crate) fn from_wire(
        receipt: AcquisitionReceipt,
        source_bytes: u64,
    ) -> Result<Self, &'static str> {
        receipt.validate(source_bytes)?;
        let AcquisitionReceipt {
            variant,
            complete,
            partial,
            truncated,
            root_id,
            relative_path,
            process,
        } = receipt;
        let state = match variant {
            SourceVariant::Inline => AcquisitionState::Inline(Completion::from_complete(complete)),
            SourceVariant::Artifact => {
                AcquisitionState::Artifact(Completion::from_complete(complete))
            }
            SourceVariant::File => AcquisitionState::File {
                root_id: root_id.ok_or("file acquisition root is missing")?,
                relative_path: PathSummary::parse(
                    relative_path
                        .as_deref()
                        .ok_or("file acquisition path is missing")?,
                )
                .ok_or("file acquisition path is invalid")?,
                completion: Completion::from_complete(complete),
            },
            SourceVariant::Process => {
                let root_id = root_id.ok_or("process acquisition root is missing")?;
                let process = process.ok_or("process acquisition data is missing")?;
                let working_path = process
                    .working_directory
                    .strip_prefix(&root_id)
                    .and_then(|value| value.strip_prefix(':'))
                    .ok_or("process working-directory identity is missing")?;
                let working_directory = PathSummary::parse(working_path)
                    .ok_or("process working-directory identity is invalid")?;
                let state = if complete {
                    ProcessState::Complete {
                        events: process.events,
                        exit_code: process
                            .exit_code
                            .ok_or("complete process exit code is missing")?,
                    }
                } else if partial {
                    let termination = match (process.exit_code, process.signal) {
                        (Some(code), None) => ProcessTermination::Exit(code),
                        (None, Some(signal)) => ProcessTermination::Signal(signal),
                        _ => return Err("process terminal state is invalid"),
                    };
                    let reason = match (process.timed_out, truncated) {
                        (true, true) => ProcessPartialReason::TimedOutAndTruncated,
                        (true, false) => ProcessPartialReason::TimedOut,
                        (false, true) => ProcessPartialReason::Truncated,
                        (false, false) => ProcessPartialReason::CaptureFailed,
                    };
                    ProcessState::Partial {
                        events: process.events,
                        termination,
                        reason,
                    }
                } else {
                    ProcessState::Failed
                };
                AcquisitionState::Process {
                    root_id,
                    working_directory,
                    state,
                }
            }
        };
        Ok(Self {
            state,
            source_bytes,
        })
    }

    pub(crate) fn inline_complete(source_bytes: u64) -> Self {
        Self::trusted(AcquisitionState::Inline(Completion::Complete), source_bytes)
    }

    pub(crate) fn inline_failed() -> Self {
        Self::trusted(AcquisitionState::Inline(Completion::Failed), 0)
    }

    pub(crate) fn artifact_replay(source_bytes: u64) -> Self {
        Self::trusted(
            AcquisitionState::Artifact(Completion::Complete),
            source_bytes,
        )
    }

    pub(crate) fn file_complete(
        root_id: &str,
        relative_path: &ByteString,
        source_bytes: u64,
    ) -> Result<Self, &'static str> {
        Self::file(root_id, relative_path, Completion::Complete, source_bytes)
    }

    pub(crate) fn file_failed(
        root_id: &str,
        relative_path: &ByteString,
    ) -> Result<Self, &'static str> {
        Self::file(root_id, relative_path, Completion::Failed, 0)
    }

    fn file(
        root_id: &str,
        relative_path: &ByteString,
        completion: Completion,
        source_bytes: u64,
    ) -> Result<Self, &'static str> {
        Self::checked(
            AcquisitionState::File {
                root_id: root_id.to_owned(),
                relative_path: PathSummary::from_bytes(relative_path)?,
                completion,
            },
            source_bytes,
        )
    }

    pub(crate) fn process_failed(
        root_id: &str,
        working_directory: &ByteString,
    ) -> Result<Self, &'static str> {
        Self::process_state(root_id, working_directory, ProcessState::Failed, 0)
    }

    pub(crate) fn process_complete(
        root_id: &str,
        working_directory: &ByteString,
        events: Vec<StreamEvent>,
        exit_code: i32,
        source_bytes: u64,
    ) -> Result<Self, &'static str> {
        Self::process_state(
            root_id,
            working_directory,
            ProcessState::Complete { events, exit_code },
            source_bytes,
        )
    }

    pub(crate) fn process_partial(
        root_id: &str,
        working_directory: &ByteString,
        events: Vec<StreamEvent>,
        termination: ProcessTermination,
        reason: ProcessPartialReason,
        source_bytes: u64,
    ) -> Result<Self, &'static str> {
        Self::process_state(
            root_id,
            working_directory,
            ProcessState::Partial {
                events,
                termination,
                reason,
            },
            source_bytes,
        )
    }

    fn process_state(
        root_id: &str,
        working_directory: &ByteString,
        state: ProcessState,
        source_bytes: u64,
    ) -> Result<Self, &'static str> {
        Self::checked(
            AcquisitionState::Process {
                root_id: root_id.to_owned(),
                working_directory: PathSummary::from_bytes(working_directory)?,
                state,
            },
            source_bytes,
        )
    }

    fn checked(state: AcquisitionState, source_bytes: u64) -> Result<Self, &'static str> {
        let acquisition = Self {
            state,
            source_bytes,
        };
        acquisition.to_receipt().validate(source_bytes)?;
        Ok(acquisition)
    }

    fn trusted(state: AcquisitionState, source_bytes: u64) -> Self {
        let acquisition = Self {
            state,
            source_bytes,
        };
        debug_assert!(acquisition.to_receipt().validate(source_bytes).is_ok());
        acquisition
    }

    pub(crate) fn to_receipt(&self) -> AcquisitionReceipt {
        self.state.to_receipt()
    }

    pub(crate) fn into_receipt(self) -> AcquisitionReceipt {
        self.state.to_receipt()
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.state.is_complete()
    }

    pub(crate) fn source_bytes(&self) -> u64 {
        self.source_bytes
    }
}

impl AcquisitionState {
    fn is_complete(&self) -> bool {
        match self {
            Self::Inline(completion) | Self::Artifact(completion) => completion.is_complete(),
            Self::File { completion, .. } => completion.is_complete(),
            Self::Process { state, .. } => matches!(state, ProcessState::Complete { .. }),
        }
    }

    fn to_receipt(&self) -> AcquisitionReceipt {
        match self {
            Self::Inline(completion) => AcquisitionReceipt {
                variant: SourceVariant::Inline,
                complete: completion.is_complete(),
                partial: false,
                truncated: false,
                root_id: None,
                relative_path: None,
                process: None,
            },
            Self::Artifact(completion) => AcquisitionReceipt {
                variant: SourceVariant::Artifact,
                complete: completion.is_complete(),
                partial: false,
                truncated: false,
                root_id: None,
                relative_path: None,
                process: None,
            },
            Self::File {
                root_id,
                relative_path,
                completion,
            } => AcquisitionReceipt {
                variant: SourceVariant::File,
                complete: completion.is_complete(),
                partial: false,
                truncated: false,
                root_id: Some(root_id.clone()),
                relative_path: Some(relative_path.render().to_owned()),
                process: None,
            },
            Self::Process {
                root_id,
                working_directory,
                state: ProcessState::Failed,
            } => AcquisitionReceipt {
                variant: SourceVariant::Process,
                complete: false,
                partial: false,
                truncated: false,
                root_id: Some(root_id.clone()),
                relative_path: None,
                process: Some(ProcessReceipt {
                    events: Vec::new(),
                    exit_code: None,
                    signal: None,
                    timed_out: false,
                    working_directory: format!("{root_id}:{}", working_directory.render()),
                }),
            },
            Self::Process {
                root_id,
                working_directory,
                state: ProcessState::Complete { events, exit_code },
            } => AcquisitionReceipt {
                variant: SourceVariant::Process,
                complete: true,
                partial: false,
                truncated: false,
                root_id: Some(root_id.clone()),
                relative_path: None,
                process: Some(ProcessReceipt {
                    events: events.clone(),
                    exit_code: Some(*exit_code),
                    signal: None,
                    timed_out: false,
                    working_directory: format!("{root_id}:{}", working_directory.render()),
                }),
            },
            Self::Process {
                root_id,
                working_directory,
                state:
                    ProcessState::Partial {
                        events,
                        termination,
                        reason,
                    },
            } => {
                let (exit_code, signal) = match termination {
                    ProcessTermination::Exit(code) => (Some(*code), None),
                    ProcessTermination::Signal(signal) => (None, Some(*signal)),
                };
                AcquisitionReceipt {
                    variant: SourceVariant::Process,
                    complete: false,
                    partial: true,
                    truncated: matches!(
                        reason,
                        ProcessPartialReason::Truncated
                            | ProcessPartialReason::TimedOutAndTruncated
                    ),
                    root_id: Some(root_id.clone()),
                    relative_path: None,
                    process: Some(ProcessReceipt {
                        events: events.clone(),
                        exit_code,
                        signal,
                        timed_out: matches!(
                            reason,
                            ProcessPartialReason::TimedOut
                                | ProcessPartialReason::TimedOutAndTruncated
                        ),
                        working_directory: format!("{root_id}:{}", working_directory.render()),
                    }),
                }
            }
        }
    }
}

pub(super) fn valid_path_summary(value: &str) -> bool {
    PathSummary::parse(value).is_some()
}
