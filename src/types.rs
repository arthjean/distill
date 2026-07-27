use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::{collections::BTreeMap, fmt, path::PathBuf};

pub const CONTRACT_VERSION: &str = "distill.context/v2";
pub const ARTIFACT_SCHEMA_VERSION: &str = "distill.artifact/v1";
pub const RECEIPT_SCHEMA_VERSION: &str = "distill.receipt/v1";
pub const RESTORE_SCHEMA_VERSION: &str = "distill.restore/v1";
pub const TRACE_SCHEMA_VERSION: &str = "distill.trace/v1";
pub const STATUS_SCHEMA_VERSION: &str = "distill.status/v2";
pub const GC_SCHEMA_VERSION: &str = "distill.gc/v2";
pub const PROJECTION_VERSION: &str = "distill.extractive/v1";
pub const POLICY_VERSION: &str = "distill.preservation/v1";
pub const CL100K_PROFILE: &str = "cl100k_base@js-tiktoken-1.0.15";
pub const MAX_LINEAGE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_ARTIFACT_LINEAGE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ByteString(pub Vec<u8>);

impl ByteString {
    #[must_use]
    pub fn from_utf8(value: impl Into<String>) -> Self {
        Self(value.into().into_bytes())
    }
}

impl From<Vec<u8>> for ByteString {
    fn from(value: Vec<u8>) -> Self {
        Self(value)
    }
}

impl From<&[u8]> for ByteString {
    fn from(value: &[u8]) -> Self {
        Self(value.to_vec())
    }
}

impl Serialize for ByteString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&BASE64.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for ByteString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        let bytes = BASE64.decode(&encoded).map_err(de::Error::custom)?;
        if BASE64.encode(&bytes) != encoded {
            return Err(de::Error::custom("byte strings require canonical base64"));
        }
        Ok(Self(bytes))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub contract_version: String,
    pub request_id: String,
    pub source: Source,
    pub budget: Budget,
    pub preservation_profile: String,
    #[serde(default)]
    pub retention: Retention,
}

impl Request {
    pub fn from_jsonl(line: &[u8]) -> Result<Self, Failure> {
        const MAX_JSONL_BYTES: usize = 16 * 1024 * 1024;
        if line.len() > MAX_JSONL_BYTES {
            return Err(Failure::new(
                FailureCode::InputTooLarge,
                "JSONL request exceeds the 16 MiB protocol limit",
            ));
        }

        #[derive(Deserialize)]
        struct CorrelationProbe<'a> {
            #[serde(borrow)]
            request_id: Option<&'a str>,
        }

        #[derive(Deserialize)]
        struct RequestProbe<'a> {
            #[serde(borrow)]
            contract_version: Option<&'a str>,
            #[serde(borrow)]
            source: Option<SourceProbe<'a>>,
        }

        #[derive(Deserialize)]
        struct SourceProbe<'a> {
            #[serde(borrow)]
            kind: Option<&'a str>,
        }

        let malformed = || {
            Failure::new(
                FailureCode::InvalidRequest,
                "JSONL request is malformed or missing required fields",
            )
        };
        let correlation: CorrelationProbe<'_> =
            serde_json::from_slice(line).map_err(|_| malformed())?;
        let correlated = |failure: Failure| match correlation.request_id.filter(|request_id| {
            !request_id.is_empty()
                && request_id.len() <= crate::request_policy::MAX_IDENTIFIER_BYTES
        }) {
            Some(request_id) => failure.for_request(request_id),
            None => failure,
        };
        let probe: RequestProbe<'_> =
            serde_json::from_slice(line).map_err(|_| correlated(malformed()))?;
        if probe
            .contract_version
            .is_some_and(|version| version != CONTRACT_VERSION)
        {
            return Err(correlated(Failure::new(
                FailureCode::SchemaUnsupported,
                "request contract version is unsupported",
            )));
        }
        if let Some(kind) = probe.source.and_then(|source| source.kind)
            && !matches!(kind, "inline" | "file" | "process" | "artifact")
        {
            return Err(correlated(Failure::new(
                FailureCode::SourceUnsupported,
                "JSONL request names an unsupported source variant",
            )));
        }
        serde_json::from_slice(line).map_err(|_| correlated(malformed()))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Source {
    Inline {
        bytes: ByteString,
        media_type: Option<String>,
    },
    File {
        root_id: String,
        relative_path: ByteString,
        binary_policy: BinaryPolicy,
    },
    Process {
        executable: ByteString,
        argv: Vec<ByteString>,
        cwd_root_id: String,
        cwd_relative_path: ByteString,
        timeout_ms: Option<u64>,
        environment_profile: Option<String>,
    },
    Artifact {
        artifact: ArtifactRef,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BinaryPolicy {
    Accept,
    Reject,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub unit: CountUnit,
    pub total_visible_limit: u64,
    pub reserved_envelope: u64,
    pub token_profile: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CountUnit {
    Bytes,
    Tokens,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Retention {
    pub expires_at: Option<u64>,
    pub ttl_seconds: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum ScalarValue {
    String(String),
    Integer(i64),
    Boolean(bool),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Outcome {
    pub visible: VisiblePayload,
    pub artifact: ArtifactRef,
    pub receipt: Receipt,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VisiblePayload {
    pub bytes: String,
    pub media_type: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    pub schema_version: String,
    pub id: String,
    pub source_sha256: String,
    pub source_bytes: u64,
    pub created_at: u64,
    pub expires_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RestoredArtifact {
    pub schema_version: String,
    pub artifact: ArtifactRef,
    pub bytes: ByteString,
    pub acquisition: AcquisitionReceipt,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactTrace {
    pub schema_version: String,
    pub artifact: ArtifactRef,
    pub acquisition: AcquisitionReceipt,
    pub receipts: Vec<Receipt>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EngineStatus {
    pub schema_version: String,
    pub store_bytes: u64,
    pub store_records: u64,
    pub expired_records: u64,
    pub max_store_bytes: u64,
    pub lineage_bytes: u64,
    pub max_lineage_bytes: u64,
    pub default_ttl_seconds: u64,
    pub max_source_bytes: u64,
    pub max_concurrent_writers: u64,
    pub root_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GarbageCollection {
    pub schema_version: String,
    pub reclaimed_bytes: u64,
    pub reclaimed_lineage_bytes: u64,
    pub reclaimed_records: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Fidelity {
    Exact,
    Extractive,
    Encoded,
    MetadataOnly,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub schema_version: String,
    pub request_id: String,
    pub source_sha256: String,
    pub artifact: ArtifactRef,
    pub projection_version: String,
    pub policy_version: String,
    pub token_profile: Option<String>,
    pub original_count: u64,
    pub visible_count: u64,
    pub count_unit: CountUnit,
    pub fidelity: Fidelity,
    pub retained_spans: Vec<ByteSpan>,
    pub omitted_spans: Vec<ByteSpan>,
    pub preservation: PreservationResult,
    pub acquisition: AcquisitionReceipt,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ByteSpan {
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PreservationResult {
    pub profile: String,
    pub mandatory_fact_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AcquisitionReceipt {
    pub variant: SourceVariant,
    pub complete: bool,
    pub partial: bool,
    pub truncated: bool,
    pub root_id: Option<String>,
    pub relative_path: Option<String>,
    pub process: Option<ProcessReceipt>,
}

impl AcquisitionReceipt {
    pub(crate) fn validate(&self, source_bytes: u64) -> Result<(), &'static str> {
        if (self.complete && self.partial)
            || (self.truncated && !self.partial)
            || (self.partial && self.variant != SourceVariant::Process)
            || (!self.complete && !self.partial && source_bytes != 0)
        {
            return Err("acquisition completion state is contradictory");
        }

        match self.variant {
            SourceVariant::Inline | SourceVariant::Artifact => {
                if self.root_id.is_some()
                    || self.relative_path.is_some()
                    || self.process.is_some()
                    || self.truncated
                {
                    return Err("acquisition fields contradict the source variant");
                }
            }
            SourceVariant::File => {
                if self
                    .root_id
                    .as_deref()
                    .is_none_or(|root| !crate::request_policy::valid_identifier(root))
                    || self
                        .relative_path
                        .as_deref()
                        .is_none_or(|path| !valid_path_summary(path))
                    || self.process.is_some()
                    || self.truncated
                {
                    return Err("file acquisition metadata is incomplete or contradictory");
                }
            }
            SourceVariant::Process => {
                let Some(root_id) = self.root_id.as_deref() else {
                    return Err("process acquisition metadata is incomplete or contradictory");
                };
                if !crate::request_policy::valid_identifier(root_id) || self.relative_path.is_some()
                {
                    return Err("process acquisition metadata is incomplete or contradictory");
                }
                let process = self
                    .process
                    .as_ref()
                    .ok_or("process acquisition data is missing")?;
                let working_path = process
                    .working_directory
                    .strip_prefix(root_id)
                    .and_then(|value| value.strip_prefix(':'));
                if working_path.is_none_or(|path| !valid_path_summary(path)) {
                    return Err("process working-directory identity is missing");
                }

                let mut expected_start = 0_u64;
                for (index, event) in process.events.iter().enumerate() {
                    if event.order != index as u64
                        || event.span.start != expected_start
                        || event.span.end <= event.span.start
                        || event.span.end > source_bytes
                    {
                        return Err("process stream events are invalid");
                    }
                    expected_start = event.span.end;
                }
                if expected_start != source_bytes {
                    return Err("process stream events do not cover the captured source");
                }

                if !self.complete && !self.partial {
                    if !process.events.is_empty()
                        || process.exit_code.is_some()
                        || process.signal.is_some()
                        || process.timed_out
                    {
                        return Err("clean process failure contains terminal capture data");
                    }
                } else if process.exit_code.is_some() == process.signal.is_some() {
                    return Err("process terminal state must contain one exit code or signal");
                }
                if self.complete && (process.timed_out || process.signal.is_some()) {
                    return Err("complete process acquisition has a failure terminal state");
                }
                if (process.timed_out || process.signal.is_some()) && !self.partial {
                    return Err("failed process terminal state is not partial");
                }
            }
        }
        Ok(())
    }
}

fn valid_path_summary(value: &str) -> bool {
    value
        .strip_prefix('<')
        .and_then(|value| value.strip_suffix(" path bytes>"))
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length <= crate::request_policy::MAX_PATH_BYTES as u64)
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceVariant {
    Inline,
    File,
    Process,
    Artifact,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProcessReceipt {
    pub events: Vec<StreamEvent>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub working_directory: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StreamEvent {
    pub order: u64,
    pub stream: ProcessStream,
    pub span: ByteSpan,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub code: FailureCode,
    pub safe_message: String,
    pub request_id: Option<String>,
    #[serde(default)]
    pub details: BTreeMap<String, ScalarValue>,
    pub artifact: Option<ArtifactRef>,
    pub acquisition: Option<AcquisitionReceipt>,
}

impl Failure {
    pub(crate) fn new(code: FailureCode, message: impl Into<String>) -> Self {
        Self {
            code,
            safe_message: message.into(),
            request_id: None,
            details: BTreeMap::new(),
            artifact: None,
            acquisition: None,
        }
    }

    pub(crate) fn for_request(mut self, request_id: &str) -> Self {
        self.request_id = Some(request_id.to_owned());
        self
    }

    pub(crate) fn with_artifact(mut self, artifact: ArtifactRef) -> Self {
        self.artifact = Some(artifact);
        self
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.safe_message)
    }
}

impl std::error::Error for Failure {}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    InvalidRequest,
    SchemaUnsupported,
    SourceUnsupported,
    TokenProfileUnsupported,
    BudgetUnsatisfiable,
    InputTooLarge,
    ResourceExhausted,
    UnsafeRoot,
    PermissionDenied,
    AcquisitionFailed,
    StoreFull,
    StoreBusy,
    CommitFailed,
    ArtifactUnknown,
    ArtifactExpired,
    ArtifactCorrupt,
    ArtifactSchemaUnsupported,
    InvariantBreach,
}

impl FailureCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::SchemaUnsupported => "schema_unsupported",
            Self::SourceUnsupported => "source_unsupported",
            Self::TokenProfileUnsupported => "token_profile_unsupported",
            Self::BudgetUnsatisfiable => "budget_unsatisfiable",
            Self::InputTooLarge => "input_too_large",
            Self::ResourceExhausted => "resource_exhausted",
            Self::UnsafeRoot => "unsafe_root",
            Self::PermissionDenied => "permission_denied",
            Self::AcquisitionFailed => "acquisition_failed",
            Self::StoreFull => "store_full",
            Self::StoreBusy => "store_busy",
            Self::CommitFailed => "commit_failed",
            Self::ArtifactUnknown => "artifact_unknown",
            Self::ArtifactExpired => "artifact_expired",
            Self::ArtifactCorrupt => "artifact_corrupt",
            Self::ArtifactSchemaUnsupported => "artifact_schema_unsupported",
            Self::InvariantBreach => "invariant_breach",
        }
    }
}

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub store_path: PathBuf,
    pub roots: BTreeMap<String, PathBuf>,
    pub environment_profiles: BTreeMap<String, BTreeMap<String, String>>,
    pub max_store_bytes: u64,
    pub max_lineage_bytes: u64,
    pub busy_timeout_ms: u64,
    pub default_ttl_seconds: u64,
}

impl EngineConfig {
    #[must_use]
    pub fn local(store_path: PathBuf) -> Self {
        Self {
            store_path,
            roots: BTreeMap::new(),
            environment_profiles: BTreeMap::new(),
            max_store_bytes: 512 * 1024 * 1024,
            max_lineage_bytes: MAX_LINEAGE_BYTES,
            busy_timeout_ms: 250,
            default_ttl_seconds: 7 * 24 * 60 * 60,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact() -> ArtifactRef {
        ArtifactRef {
            schema_version: ARTIFACT_SCHEMA_VERSION.to_owned(),
            id: "a".repeat(32),
            source_sha256: "b".repeat(64),
            source_bytes: 3,
            created_at: 1,
            expires_at: 2,
        }
    }

    #[test]
    fn acquisition_semantics_cover_runtime_states_and_reject_contradictions() {
        let inline = AcquisitionReceipt {
            variant: SourceVariant::Inline,
            complete: true,
            partial: false,
            truncated: false,
            root_id: None,
            relative_path: None,
            process: None,
        };
        inline.validate(3).expect("complete inline");
        AcquisitionReceipt {
            complete: false,
            ..inline.clone()
        }
        .validate(0)
        .expect("clean inline failure");

        let file = AcquisitionReceipt {
            variant: SourceVariant::File,
            complete: true,
            partial: false,
            truncated: false,
            root_id: Some("workspace".to_owned()),
            relative_path: Some("<4 path bytes>".to_owned()),
            process: None,
        };
        file.validate(3).expect("complete file");
        AcquisitionReceipt {
            complete: false,
            ..file
        }
        .validate(0)
        .expect("clean file failure");

        let process = AcquisitionReceipt {
            variant: SourceVariant::Process,
            complete: true,
            partial: false,
            truncated: false,
            root_id: Some("workspace".to_owned()),
            relative_path: None,
            process: Some(ProcessReceipt {
                events: vec![StreamEvent {
                    order: 0,
                    stream: ProcessStream::Stdout,
                    span: ByteSpan { start: 0, end: 3 },
                }],
                exit_code: Some(0),
                signal: None,
                timed_out: false,
                working_directory: "workspace:<0 path bytes>".to_owned(),
            }),
        };
        process.validate(3).expect("complete process");
        AcquisitionReceipt {
            complete: false,
            partial: true,
            truncated: false,
            process: Some(ProcessReceipt {
                exit_code: None,
                signal: Some(15),
                timed_out: true,
                ..process.process.clone().expect("process")
            }),
            ..process.clone()
        }
        .validate(3)
        .expect("timed-out partial process");
        AcquisitionReceipt {
            complete: false,
            partial: true,
            truncated: true,
            process: Some(ProcessReceipt {
                exit_code: None,
                signal: Some(9),
                ..process.process.clone().expect("process")
            }),
            ..process.clone()
        }
        .validate(3)
        .expect("truncated partial process");
        AcquisitionReceipt {
            complete: false,
            partial: true,
            process: Some(ProcessReceipt {
                exit_code: Some(1),
                signal: None,
                ..process.process.clone().expect("process")
            }),
            ..process.clone()
        }
        .validate(3)
        .expect("partial process read failure");
        AcquisitionReceipt {
            complete: false,
            process: Some(ProcessReceipt {
                events: Vec::new(),
                exit_code: None,
                signal: None,
                timed_out: false,
                working_directory: "workspace:<0 path bytes>".to_owned(),
            }),
            ..process.clone()
        }
        .validate(0)
        .expect("clean process failure");

        AcquisitionReceipt {
            variant: SourceVariant::Artifact,
            ..inline.clone()
        }
        .validate(3)
        .expect("artifact replay");

        let mut contradictions = Vec::new();
        contradictions.push(AcquisitionReceipt {
            partial: true,
            ..inline.clone()
        });
        contradictions.push(AcquisitionReceipt {
            truncated: true,
            ..inline
        });
        contradictions.push(AcquisitionReceipt {
            process: None,
            ..process.clone()
        });
        contradictions.push(AcquisitionReceipt {
            root_id: None,
            ..process.clone()
        });
        contradictions.push(AcquisitionReceipt {
            process: Some(ProcessReceipt {
                events: vec![StreamEvent {
                    order: 1,
                    stream: ProcessStream::Stdout,
                    span: ByteSpan { start: 0, end: 3 },
                }],
                ..process.process.clone().expect("process")
            }),
            ..process.clone()
        });
        contradictions.push(AcquisitionReceipt {
            process: Some(ProcessReceipt {
                events: vec![
                    StreamEvent {
                        order: 0,
                        stream: ProcessStream::Stdout,
                        span: ByteSpan { start: 0, end: 2 },
                    },
                    StreamEvent {
                        order: 1,
                        stream: ProcessStream::Stderr,
                        span: ByteSpan { start: 1, end: 3 },
                    },
                ],
                ..process.process.clone().expect("process")
            }),
            ..process.clone()
        });
        contradictions.push(AcquisitionReceipt {
            process: Some(ProcessReceipt {
                events: vec![StreamEvent {
                    order: 0,
                    stream: ProcessStream::Stdout,
                    span: ByteSpan { start: 0, end: 4 },
                }],
                ..process.process.expect("process")
            }),
            ..process
        });
        for receipt in contradictions {
            assert!(receipt.validate(3).is_err());
        }
    }

    #[test]
    fn acquisition_semantics_reject_each_invalid_metadata_shape() {
        let inline = AcquisitionReceipt {
            variant: SourceVariant::Inline,
            complete: true,
            partial: false,
            truncated: false,
            root_id: None,
            relative_path: None,
            process: None,
        };
        let file = AcquisitionReceipt {
            variant: SourceVariant::File,
            complete: true,
            partial: false,
            truncated: false,
            root_id: Some("workspace".to_owned()),
            relative_path: Some("<4 path bytes>".to_owned()),
            process: None,
        };
        let process = AcquisitionReceipt {
            variant: SourceVariant::Process,
            complete: true,
            partial: false,
            truncated: false,
            root_id: Some("workspace".to_owned()),
            relative_path: None,
            process: Some(ProcessReceipt {
                events: vec![StreamEvent {
                    order: 0,
                    stream: ProcessStream::Stdout,
                    span: ByteSpan { start: 0, end: 3 },
                }],
                exit_code: Some(0),
                signal: None,
                timed_out: false,
                working_directory: "workspace:<0 path bytes>".to_owned(),
            }),
        };

        let invalid_receipts = [
            (
                AcquisitionReceipt {
                    complete: false,
                    partial: true,
                    ..inline.clone()
                },
                3,
            ),
            (
                AcquisitionReceipt {
                    complete: false,
                    ..inline.clone()
                },
                1,
            ),
            (
                AcquisitionReceipt {
                    root_id: Some("workspace".to_owned()),
                    ..inline.clone()
                },
                3,
            ),
            (
                AcquisitionReceipt {
                    relative_path: Some("<0 path bytes>".to_owned()),
                    ..inline.clone()
                },
                3,
            ),
            (
                AcquisitionReceipt {
                    process: process.process.clone(),
                    ..inline
                },
                3,
            ),
            (
                AcquisitionReceipt {
                    root_id: None,
                    ..file.clone()
                },
                3,
            ),
            (
                AcquisitionReceipt {
                    root_id: Some(String::new()),
                    ..file.clone()
                },
                3,
            ),
            (
                AcquisitionReceipt {
                    relative_path: None,
                    ..file.clone()
                },
                3,
            ),
            (
                AcquisitionReceipt {
                    relative_path: Some("4 path bytes>".to_owned()),
                    ..file.clone()
                },
                3,
            ),
            (
                AcquisitionReceipt {
                    relative_path: Some("<4 path bytes".to_owned()),
                    ..file.clone()
                },
                3,
            ),
            (
                AcquisitionReceipt {
                    relative_path: Some("< path bytes>".to_owned()),
                    ..file.clone()
                },
                3,
            ),
            (
                AcquisitionReceipt {
                    relative_path: Some("<four path bytes>".to_owned()),
                    ..file.clone()
                },
                3,
            ),
            (
                AcquisitionReceipt {
                    relative_path: Some(format!(
                        "<{} path bytes>",
                        crate::request_policy::MAX_PATH_BYTES as u64 + 1
                    )),
                    ..file.clone()
                },
                3,
            ),
            (
                AcquisitionReceipt {
                    process: process.process.clone(),
                    ..file
                },
                3,
            ),
            (
                AcquisitionReceipt {
                    root_id: Some(String::new()),
                    ..process.clone()
                },
                3,
            ),
            (
                AcquisitionReceipt {
                    relative_path: Some("<0 path bytes>".to_owned()),
                    ..process.clone()
                },
                3,
            ),
            (
                AcquisitionReceipt {
                    process: Some(ProcessReceipt {
                        working_directory: "other:<0 path bytes>".to_owned(),
                        ..process.process.clone().expect("process")
                    }),
                    ..process.clone()
                },
                3,
            ),
            (
                AcquisitionReceipt {
                    process: Some(ProcessReceipt {
                        working_directory: "workspace:invalid".to_owned(),
                        ..process.process.clone().expect("process")
                    }),
                    ..process.clone()
                },
                3,
            ),
            (
                AcquisitionReceipt {
                    process: Some(ProcessReceipt {
                        events: vec![StreamEvent {
                            order: 0,
                            stream: ProcessStream::Stdout,
                            span: ByteSpan { start: 0, end: 0 },
                        }],
                        ..process.process.clone().expect("process")
                    }),
                    ..process.clone()
                },
                0,
            ),
            (
                AcquisitionReceipt {
                    process: Some(ProcessReceipt {
                        events: Vec::new(),
                        ..process.process.clone().expect("process")
                    }),
                    ..process.clone()
                },
                3,
            ),
            (
                AcquisitionReceipt {
                    complete: false,
                    process: Some(ProcessReceipt {
                        events: Vec::new(),
                        exit_code: Some(1),
                        signal: None,
                        timed_out: false,
                        ..process.process.clone().expect("process")
                    }),
                    ..process.clone()
                },
                0,
            ),
            (
                AcquisitionReceipt {
                    complete: false,
                    process: Some(ProcessReceipt {
                        events: Vec::new(),
                        exit_code: None,
                        signal: Some(15),
                        timed_out: false,
                        ..process.process.clone().expect("process")
                    }),
                    ..process.clone()
                },
                0,
            ),
            (
                AcquisitionReceipt {
                    complete: false,
                    process: Some(ProcessReceipt {
                        events: Vec::new(),
                        exit_code: None,
                        signal: None,
                        timed_out: true,
                        ..process.process.clone().expect("process")
                    }),
                    ..process.clone()
                },
                0,
            ),
            (
                AcquisitionReceipt {
                    process: Some(ProcessReceipt {
                        exit_code: None,
                        signal: None,
                        ..process.process.clone().expect("process")
                    }),
                    ..process.clone()
                },
                3,
            ),
            (
                AcquisitionReceipt {
                    process: Some(ProcessReceipt {
                        exit_code: Some(1),
                        signal: Some(15),
                        ..process.process.clone().expect("process")
                    }),
                    ..process.clone()
                },
                3,
            ),
            (
                AcquisitionReceipt {
                    process: Some(ProcessReceipt {
                        timed_out: true,
                        ..process.process.clone().expect("process")
                    }),
                    ..process.clone()
                },
                3,
            ),
            (
                AcquisitionReceipt {
                    process: Some(ProcessReceipt {
                        exit_code: None,
                        signal: Some(15),
                        ..process.process.expect("process")
                    }),
                    ..process
                },
                3,
            ),
        ];

        for (receipt, source_bytes) in invalid_receipts {
            assert!(
                receipt.validate(source_bytes).is_err(),
                "invalid receipt was accepted: {receipt:?}"
            );
        }
    }

    #[test]
    fn jsonl_request_decoding_is_bounded_strict_and_versioned() {
        let request = Request {
            contract_version: CONTRACT_VERSION.to_owned(),
            request_id: "round-trip".to_owned(),
            source: Source::Inline {
                bytes: ByteString::from(&[0, 1, 255][..]),
                media_type: None,
            },
            budget: Budget {
                unit: CountUnit::Bytes,
                total_visible_limit: 32,
                reserved_envelope: 4,
                token_profile: None,
            },
            preservation_profile: "plain-text/v1".to_owned(),
            retention: Retention::default(),
        };
        let mut line = serde_json::to_vec(&request).expect("serialize");
        line.push(b'\n');
        assert_eq!(Request::from_jsonl(&line).expect("decode"), request);

        let mut v1 = serde_json::to_value(&request).expect("value");
        v1["contract_version"] = serde_json::json!("distill.context/v1");
        v1["metadata"] = serde_json::json!({});
        let failure = Request::from_jsonl(&serde_json::to_vec(&v1).expect("serialize v1"))
            .expect_err("v1 schema");
        assert_eq!(failure.code, FailureCode::SchemaUnsupported);
        assert_eq!(failure.request_id.as_deref(), Some("round-trip"));

        let mut removed_metadata = serde_json::to_value(&request).expect("value");
        removed_metadata["metadata"] = serde_json::json!({});
        let failure = Request::from_jsonl(
            &serde_json::to_vec(&removed_metadata).expect("serialize metadata"),
        )
        .expect_err("removed metadata");
        assert_eq!(failure.code, FailureCode::InvalidRequest);
        assert_eq!(failure.request_id.as_deref(), Some("round-trip"));

        let mut invalid_typed_field = serde_json::to_value(&request).expect("value");
        invalid_typed_field["budget"]["total_visible_limit"] = serde_json::json!("invalid");
        let failure = Request::from_jsonl(
            &serde_json::to_vec(&invalid_typed_field).expect("serialize typed failure"),
        )
        .expect_err("typed failure");
        assert_eq!(failure.code, FailureCode::InvalidRequest);
        assert_eq!(failure.request_id.as_deref(), Some("round-trip"));

        let mut missing = serde_json::to_value(&request).expect("value");
        missing.as_object_mut().expect("object").remove("budget");
        let missing_failure =
            Request::from_jsonl(&serde_json::to_vec(&missing).expect("serialize missing field"))
                .expect_err("missing field");
        let mut unknown = serde_json::to_value(&request).expect("value");
        unknown["unknown"] = serde_json::json!(true);
        let unknown_failure =
            Request::from_jsonl(&serde_json::to_vec(&unknown).expect("serialize unknown field"))
                .expect_err("unknown field");
        assert_eq!(missing_failure.code, FailureCode::InvalidRequest);
        assert_eq!(missing_failure.request_id.as_deref(), Some("round-trip"));
        assert_eq!(unknown_failure.code, missing_failure.code);
        assert_eq!(unknown_failure.safe_message, missing_failure.safe_message);
        assert_eq!(unknown_failure.request_id, missing_failure.request_id);

        let malformed =
            Request::from_jsonl(br#"{"request_id":"unfinished"#).expect_err("malformed request");
        assert_eq!(malformed.code, FailureCode::InvalidRequest);
        assert!(malformed.request_id.is_none());
        assert_eq!(
            {
                let oversized =
                    Request::from_jsonl(&vec![b'x'; 16 * 1024 * 1024 + 1]).expect_err("oversized");
                assert!(oversized.request_id.is_none());
                assert_eq!(
                    oversized.safe_message,
                    "JSONL request exceeds the 16 MiB protocol limit"
                );
                oversized.code
            },
            FailureCode::InputTooLarge,
        );

        let mut unsupported = serde_json::to_value(&request).expect("value");
        unsupported["source"]["kind"] = serde_json::Value::String("remote_url".to_owned());
        let failure =
            Request::from_jsonl(&serde_json::to_vec(&unsupported).expect("unknown source"))
                .expect_err("unsupported source");
        assert_eq!(failure.code, FailureCode::SourceUnsupported);
        assert_eq!(failure.request_id.as_deref(), Some("round-trip"));
    }

    #[test]
    fn jsonl_request_without_source_is_invalid() {
        let failure = Request::from_jsonl(b"{}").expect_err("missing source");

        assert_eq!(failure.code, FailureCode::InvalidRequest);
    }

    #[test]
    fn every_source_variant_and_failure_round_trips() {
        let variants = [
            Source::Inline {
                bytes: ByteString::from(&[0, 255][..]),
                media_type: Some("application/octet-stream".to_owned()),
            },
            Source::File {
                root_id: "root".to_owned(),
                relative_path: ByteString::from_utf8("src/lib.rs"),
                binary_policy: BinaryPolicy::Reject,
            },
            Source::Process {
                executable: ByteString::from_utf8("/usr/bin/printf"),
                argv: vec![ByteString::from_utf8("--literal")],
                cwd_root_id: "root".to_owned(),
                cwd_relative_path: ByteString::from_utf8("workspace"),
                timeout_ms: Some(1_000),
                environment_profile: Some("safe".to_owned()),
            },
            Source::Artifact {
                artifact: artifact(),
            },
        ];
        for source in variants {
            let encoded = serde_json::to_vec(&source).expect("source JSON");
            assert_eq!(
                serde_json::from_slice::<Source>(&encoded).expect("source decode"),
                source
            );
        }

        let failure = Failure {
            code: FailureCode::ArtifactCorrupt,
            safe_message: "integrity failure".to_owned(),
            request_id: Some("request".to_owned()),
            details: BTreeMap::from([
                ("retryable".to_owned(), ScalarValue::Boolean(false)),
                ("attempt".to_owned(), ScalarValue::Integer(1)),
            ]),
            artifact: Some(artifact()),
            acquisition: None,
        };
        let encoded = serde_json::to_vec(&failure).expect("failure JSON");
        assert_eq!(
            serde_json::from_slice::<Failure>(&encoded).expect("failure decode"),
            failure
        );
    }

    #[test]
    fn noncanonical_base64_is_rejected() {
        assert!(serde_json::from_str::<ByteString>("\"YQ\"").is_err());
    }
}
