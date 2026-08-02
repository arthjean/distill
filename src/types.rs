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
            !request_id.is_empty() && request_id.len() <= crate::contract::MAX_IDENTIFIER_BYTES
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
        /// Advisory source metadata in contract v2. It does not alter projection semantics.
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
                    .is_none_or(|root| !crate::contract::valid_identifier(root))
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
                if !crate::contract::valid_identifier(root_id) || self.relative_path.is_some() {
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

mod acquisition;
use acquisition::valid_path_summary;
pub(crate) use acquisition::{ProcessPartialReason, ProcessTermination, ValidatedAcquisition};

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
#[path = "types/tests.rs"]
mod tests;
