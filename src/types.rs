use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::{collections::BTreeMap, fmt, path::PathBuf};

pub const CONTRACT_VERSION: &str = "distill.context/v3";
/// The preceding contract stays accepted unchanged. v3 adds only the optional
/// artifact selector, so a v2 request that names none behaves identically.
pub const CONTRACT_VERSION_V2: &str = "distill.context/v2";

#[must_use]
pub fn supported_contract_version(version: &str) -> bool {
    version == CONTRACT_VERSION || version == CONTRACT_VERSION_V2
}
pub const ARTIFACT_SCHEMA_VERSION: &str = "distill.artifact/v1";
pub const RECEIPT_SCHEMA_VERSION: &str = "distill.receipt/v2";
/// The preceding receipt schema stays readable. v2 only adds the applied policy
/// and the collapsed-run annotations, so lineage recorded before this release
/// still verifies, still traces, and is never rewritten.
pub const RECEIPT_SCHEMA_VERSION_V1: &str = "distill.receipt/v1";
pub const RESTORE_SCHEMA_VERSION: &str = "distill.restore/v1";
pub const TRACE_SCHEMA_VERSION: &str = "distill.trace/v1";
pub const STATUS_SCHEMA_VERSION: &str = "distill.status/v2";
pub const GC_SCHEMA_VERSION: &str = "distill.gc/v2";
pub const PROJECTION_VERSION: &str = "distill.extractive/v1";
/// Preservation policy derived from the detected shape of the observation. The
/// preceding version identifies the retired needle table, and receipts that
/// carry it stay readable.
pub const POLICY_VERSION: &str = "distill.preservation/v2";
pub const POLICY_VERSION_V1: &str = "distill.preservation/v1";
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
            .is_some_and(|version| !supported_contract_version(version))
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
        /// Bounded retrieval over the committed source, added by
        /// `distill.context/v3`. Absent means the whole artifact is projected.
        #[serde(default)]
        selector: Option<ArtifactSelector>,
    },
}

/// A bounded region request over a committed artifact. Both variants describe
/// data, never a pattern language: literal text only.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ArtifactSelector {
    Lines {
        start_line: u64,
        line_count: u64,
    },
    Pattern {
        pattern: ByteString,
        #[serde(default)]
        before_lines: Option<u64>,
        #[serde(default)]
        after_lines: Option<u64>,
        #[serde(default)]
        max_matches: Option<u64>,
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
    /// The preservation profile the request named.
    pub profile: String,
    /// The policy that ran. Under `auto/v1` it names the detected shape, so the
    /// receipt records both what was asked and what was applied. Receipts
    /// written before the shape policies carry an empty value.
    #[serde(default)]
    pub applied_profile: String,
    pub mandatory_fact_ids: Vec<String>,
    /// Runs of redundant lines the visible payload states as collapsed. They
    /// are the only visible content that is not a verbatim source slice, they
    /// stay inside `omitted_spans`, and they never change the partition.
    #[serde(default)]
    pub aggregates: Vec<AggregateSpan>,
}

/// One collapsed run: the source range the payload replaced with an annotation,
/// and the number of lines that annotation reports.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AggregateSpan {
    pub span: ByteSpan,
    pub lines: u64,
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
        ValidatedAcquisition::from_wire(self.clone(), source_bytes).map(|_| ())
    }
}

mod acquisition;
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
