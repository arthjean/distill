#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
// Failure is the stable, serializable boundary contract. Boxing its receipt and
// artifact fields would change the public Rust API for an error-path size win.
#![allow(clippy::result_large_err)]

mod artifact;
mod contract;
mod projection;
mod request_policy;
mod runtime;
mod types;

pub use contract::{MAX_IDENTIFIER_BYTES, MAX_PATH_BYTES};
pub use request_policy::{
    MAX_PROCESS_ARGUMENT_BYTES, MAX_PROCESS_ARGUMENTS, MAX_PROCESS_EXECUTABLE_BYTES,
    MAX_PROCESS_TIMEOUT_MS, MAX_SOURCE_BYTES, MIN_PROCESS_TIMEOUT_MS,
};
pub use types::{
    ARTIFACT_SCHEMA_VERSION, AcquisitionReceipt, ArtifactRef, ArtifactTrace, BinaryPolicy, Budget,
    ByteSpan, ByteString, CL100K_PROFILE, CONTRACT_VERSION, CountUnit, EngineConfig, EngineStatus,
    Failure, FailureCode, Fidelity, GC_SCHEMA_VERSION, GarbageCollection,
    MAX_ARTIFACT_LINEAGE_BYTES, MAX_LINEAGE_BYTES, Outcome, POLICY_VERSION, PROJECTION_VERSION,
    PreservationResult, ProcessReceipt, ProcessStream, RECEIPT_SCHEMA_VERSION,
    RESTORE_SCHEMA_VERSION, Receipt, Request, RestoredArtifact, Retention, STATUS_SCHEMA_VERSION,
    ScalarValue, Source, SourceVariant, StreamEvent, TRACE_SCHEMA_VERSION, VisiblePayload,
};

use artifact::ArtifactStore;
use projection::{Projection, ProjectionSpec};
#[cfg(test)]
use runtime::FixtureRuntime;
use runtime::{Acquired, LocalRuntime, ProductionRuntime, failure_receipt};
use std::{collections::BTreeMap, path::Component, sync::Arc};
use types::ValidatedAcquisition;

pub struct Engine {
    runtime: Arc<dyn LocalRuntime>,
    store: ArtifactStore,
    config: EngineConfig,
}

impl Engine {
    pub fn new(config: EngineConfig) -> Result<Self, Failure> {
        validate_config(&config)?;
        let runtime = ProductionRuntime::new(&config)?;
        let store = ArtifactStore::new(
            config.store_path.clone(),
            config.max_store_bytes,
            config.max_lineage_bytes,
            config.busy_timeout_ms,
        );
        store.initialize()?;
        Ok(Self {
            runtime: Arc::new(runtime),
            store,
            config,
        })
    }

    pub fn handle(&self, request: Request) -> Result<Outcome, Failure> {
        let outcome = self.handle_inner(request)?;
        let request_id = outcome.receipt.request_id.clone();
        self.store
            .record_receipt(&outcome.artifact, &outcome.receipt)
            .map_err(|failure| {
                failure
                    .with_artifact(outcome.artifact.clone())
                    .for_request(&request_id)
            })?;
        Ok(outcome)
    }

    pub fn resolve_artifact(&self, id: &str) -> Result<ArtifactRef, Failure> {
        let now = self.runtime.now()?;
        self.store.reference_by_id(id, now)
    }

    pub fn restore(&self, artifact: &ArtifactRef) -> Result<RestoredArtifact, Failure> {
        let now = self.runtime.now()?;
        let stored = self.store.retrieve(artifact, now)?;
        Ok(RestoredArtifact {
            schema_version: RESTORE_SCHEMA_VERSION.to_owned(),
            artifact: artifact.clone(),
            bytes: stored.bytes.into(),
            acquisition: stored.acquisition.into_receipt(),
        })
    }

    pub fn trace(&self, artifact: &ArtifactRef) -> Result<ArtifactTrace, Failure> {
        let now = self.runtime.now()?;
        let trace = self.store.trace(artifact, now)?;
        Ok(ArtifactTrace {
            schema_version: TRACE_SCHEMA_VERSION.to_owned(),
            artifact: artifact.clone(),
            acquisition: trace.acquisition,
            receipts: trace.receipts,
        })
    }

    pub fn status(&self) -> Result<EngineStatus, Failure> {
        let now = self.runtime.now()?;
        let status = self.store.status(now)?;
        Ok(EngineStatus {
            schema_version: STATUS_SCHEMA_VERSION.to_owned(),
            store_bytes: status.bytes,
            store_records: status.records,
            expired_records: status.expired_records,
            max_store_bytes: self.config.max_store_bytes,
            lineage_bytes: status.lineage_bytes,
            max_lineage_bytes: self.config.max_lineage_bytes,
            default_ttl_seconds: self.config.default_ttl_seconds,
            max_source_bytes: MAX_SOURCE_BYTES as u64,
            max_concurrent_writers: 8,
            root_ids: self.config.roots.keys().cloned().collect(),
        })
    }

    pub fn collect_garbage(&self) -> Result<GarbageCollection, Failure> {
        let now = self.runtime.now()?;
        let report = self.store.collect_garbage(now)?;
        Ok(GarbageCollection {
            schema_version: GC_SCHEMA_VERSION.to_owned(),
            reclaimed_bytes: report.reclaimed_bytes,
            reclaimed_lineage_bytes: report.reclaimed_lineage_bytes,
            reclaimed_records: report.reclaimed_records,
        })
    }

    fn handle_inner(&self, request: Request) -> Result<Outcome, Failure> {
        let request = request_policy::prepare(request, &self.config)?;
        let projection_spec = request.projection;
        let now = self
            .runtime
            .now()
            .map_err(|failure| failure.for_request(&request.request_id))?;
        let expires_at = request
            .retention
            .resolve(now, self.config.default_ttl_seconds)
            .map_err(|failure| failure.for_request(&request.request_id))?;

        let (acquired, artifact) = match &request.source {
            request_policy::ValidatedSource::Artifact(artifact) => {
                let stored = self
                    .store
                    .retrieve(artifact, now)
                    .map_err(|failure| failure.for_request(&request.request_id))?;
                if !stored.acquisition.is_complete() {
                    return Err(Failure {
                        code: FailureCode::AcquisitionFailed,
                        safe_message:
                            "partial artifact is diagnosis-only and cannot be projected as complete"
                                .to_owned(),
                        request_id: Some(request.request_id.clone()),
                        details: BTreeMap::new(),
                        artifact: Some(artifact.clone()),
                        acquisition: Some(stored.acquisition.into_receipt()),
                    });
                }
                let receipt = ValidatedAcquisition::new(
                    AcquisitionReceipt {
                        variant: SourceVariant::Artifact,
                        complete: true,
                        partial: false,
                        truncated: false,
                        root_id: None,
                        relative_path: None,
                        process: None,
                    },
                    stored.bytes.len() as u64,
                )
                .map_err(|message| {
                    Failure::new(FailureCode::InvariantBreach, message)
                        .for_request(&request.request_id)
                })?;
                (
                    Acquired {
                        bytes: stored.bytes,
                        receipt,
                    },
                    artifact.clone(),
                )
            }
            request_policy::ValidatedSource::Local(source) => {
                let acquired = match self.runtime.acquire(source) {
                    Ok(acquired) => acquired,
                    Err(mut acquisition_error) => {
                        let failure = acquisition_error.failure.for_request(&request.request_id);
                        let Some(partial) = acquisition_error.partial.take() else {
                            let acquisition = failure_receipt(source)
                                .map_err(|failure| failure.for_request(&request.request_id))?;
                            return Err(Failure {
                                acquisition: Some(acquisition.into_receipt()),
                                ..failure
                            });
                        };
                        let artifact =
                            self.commit_source(&partial, now, expires_at, &request.request_id)?;
                        return Err(Failure {
                            artifact: Some(artifact),
                            acquisition: Some(partial.receipt.into_receipt()),
                            ..failure
                        });
                    }
                };
                let artifact =
                    self.commit_source(&acquired, now, expires_at, &request.request_id)?;
                (acquired, artifact)
            }
        };

        let projection =
            projection::project_validated(&acquired.bytes, projection_spec).map_err(|failure| {
                let failure = failure.for_request(&request.request_id);
                if failure.code == FailureCode::BudgetUnsatisfiable {
                    failure.with_artifact(artifact.clone())
                } else {
                    failure
                }
            })?;
        build_outcome(request, acquired, artifact, projection, projection_spec)
    }

    fn commit_source(
        &self,
        acquired: &Acquired,
        now: u64,
        expires_at: u64,
        request_id: &str,
    ) -> Result<ArtifactRef, Failure> {
        let id = self
            .runtime
            .random_id()
            .map_err(|failure| failure.for_request(request_id))?;
        self.store
            .commit(&id, &acquired.bytes, &acquired.receipt, now, expires_at)
            .map_err(|failure| failure.for_request(request_id))
    }

    #[cfg(test)]
    fn fixture(config: EngineConfig, now: u64) -> Result<Self, Failure> {
        validate_config(&config)?;
        let store = ArtifactStore::new(
            config.store_path.clone(),
            config.max_store_bytes,
            config.max_lineage_bytes,
            config.busy_timeout_ms,
        );
        store.initialize()?;
        Ok(Self {
            runtime: Arc::new(FixtureRuntime::new(now)),
            store,
            config,
        })
    }
}

fn build_outcome(
    request: request_policy::ValidatedRequest,
    acquired: Acquired,
    artifact: ArtifactRef,
    projection: Projection,
    projection_spec: ProjectionSpec,
) -> Result<Outcome, Failure> {
    let receipt = Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION.to_owned(),
        request_id: request.request_id,
        source_sha256: artifact.source_sha256.clone(),
        artifact: artifact.clone(),
        projection_version: PROJECTION_VERSION.to_owned(),
        policy_version: POLICY_VERSION.to_owned(),
        token_profile: projection_spec.token_profile(),
        original_count: projection.original_count,
        visible_count: projection.visible_count,
        count_unit: projection_spec.unit(),
        fidelity: projection.fidelity,
        retained_spans: projection.retained_spans,
        omitted_spans: projection.omitted_spans,
        preservation: PreservationResult {
            profile: projection_spec.profile().to_owned(),
            mandatory_fact_ids: projection.mandatory_fact_ids,
        },
        acquisition: acquired.receipt.into_receipt(),
    };
    receipt
        .acquisition
        .validate(receipt.artifact.source_bytes)
        .map_err(|message| {
            Failure::new(FailureCode::InvariantBreach, message).for_request(&receipt.request_id)
        })?;
    if !projection_spec.accounts_for(receipt.visible_count) {
        return Err(Failure::new(
            FailureCode::InvariantBreach,
            "receipt accounting exceeds the declared budget",
        )
        .for_request(&receipt.request_id));
    }
    Ok(Outcome {
        visible: VisiblePayload {
            bytes: projection.visible,
            media_type: "text/plain".to_owned(),
        },
        artifact,
        receipt,
    })
}

fn validate_config(config: &EngineConfig) -> Result<(), Failure> {
    let normalized_absolute = |path: &std::path::Path| {
        path.is_absolute()
            && path
                .components()
                .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
    };
    if !normalized_absolute(&config.store_path) {
        return Err(Failure::new(
            FailureCode::UnsafeRoot,
            "artifact store path must be normalized and absolute",
        ));
    }
    let store_root = config.store_path.parent().ok_or_else(|| {
        Failure::new(
            FailureCode::UnsafeRoot,
            "artifact store path has no parent directory",
        )
    })?;
    for root in config.roots.values() {
        if !normalized_absolute(root) {
            return Err(Failure::new(
                FailureCode::UnsafeRoot,
                "acquisition roots must be normalized and absolute",
            ));
        }
        if store_root.starts_with(root) || root.starts_with(store_root) {
            return Err(Failure::new(
                FailureCode::UnsafeRoot,
                "artifact storage must not overlap an acquisition root",
            ));
        }
    }
    if config.max_store_bytes == 0 {
        return Err(Failure::new(
            FailureCode::InvalidRequest,
            "artifact store cap must be positive",
        ));
    }
    if config.max_lineage_bytes == 0 || config.max_lineage_bytes > MAX_LINEAGE_BYTES {
        return Err(Failure::new(
            FailureCode::InvalidRequest,
            "lineage cap must be positive and no greater than 64 MiB",
        ));
    }
    if config.busy_timeout_ms == 0 || config.busy_timeout_ms > 5_000 {
        return Err(Failure::new(
            FailureCode::InvalidRequest,
            "artifact busy timeout must be from 1 ms through 5 seconds",
        ));
    }
    if config.default_ttl_seconds == 0 {
        return Err(Failure::new(
            FailureCode::InvalidRequest,
            "default artifact retention must be positive",
        ));
    }
    Ok(())
}

#[cfg(feature = "fuzzing")]
pub fn fuzz_one(data: &[u8]) {
    if data.len() <= 16 * 1024 * 1024 {
        let _request = Request::from_jsonl(data);
        let _artifact = serde_json::from_slice::<ArtifactRef>(data);
        let _acquisition = serde_json::from_slice::<AcquisitionReceipt>(data);
    }
    projection::fuzz_projection(data);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn request(bytes: &[u8], limit: u64) -> Request {
        Request {
            contract_version: CONTRACT_VERSION.to_owned(),
            request_id: "request-1".to_owned(),
            source: Source::Inline {
                bytes: ByteString::from(bytes),
                media_type: Some("text/plain".to_owned()),
            },
            budget: Budget {
                unit: CountUnit::Bytes,
                total_visible_limit: limit,
                reserved_envelope: 0,
                token_profile: None,
            },
            preservation_profile: "plain-text/v1".to_owned(),
            retention: Retention::default(),
        }
    }

    fn fixture() -> (TempDir, Engine) {
        let directory = tempfile::tempdir().expect("temp directory");
        let config = EngineConfig::local(directory.path().join("store/store.sqlite"));
        let engine = Engine::fixture(config, 1_000).expect("fixture engine");
        (directory, engine)
    }

    #[test]
    fn exact_content_round_trips_through_artifact_source() {
        let (_directory, engine) = fixture();
        let first = engine
            .handle(request(b"exact content", 64))
            .expect("first outcome");
        assert_eq!(first.visible.bytes, "exact content");
        assert_eq!(first.receipt.fidelity, Fidelity::Exact);

        let mut second_request = request(b"ignored", 64);
        second_request.request_id = "request-2".to_owned();
        second_request.source = Source::Artifact {
            artifact: first.artifact.clone(),
        };
        let second = engine.handle(second_request).expect("artifact outcome");
        assert_eq!(second.visible.bytes, "exact content");
        assert_eq!(second.receipt.acquisition.variant, SourceVariant::Artifact);
        assert_eq!(second.artifact, first.artifact);
        let trace = engine.trace(&first.artifact).expect("artifact trace");
        assert_eq!(
            trace
                .receipts
                .iter()
                .map(|receipt| receipt.request_id.as_str())
                .collect::<Vec<_>>(),
            vec!["request-1", "request-2"]
        );
    }

    #[test]
    fn impossible_budget_returns_committed_artifact() {
        let (_directory, engine) = fixture();
        let mut candidate = request(b"ERROR critical\nnoise\n", 2);
        candidate.preservation_profile = "build-log/v1".to_owned();
        let failure = engine.handle(candidate).expect_err("budget failure");
        assert_eq!(failure.code, FailureCode::BudgetUnsatisfiable);
        assert!(failure.artifact.is_some());
    }

    #[test]
    fn validation_failures_are_distinct_and_precede_capture() {
        let (_directory, engine) = fixture();
        let mut schema = request(b"x", 1);
        schema.contract_version = "distill.context/v1".to_owned();
        assert_eq!(
            engine.handle(schema).expect_err("schema").code,
            FailureCode::SchemaUnsupported
        );

        let mut token = request(b"x", 1);
        token.budget.unit = CountUnit::Tokens;
        token.budget.token_profile = Some("unknown@v1".to_owned());
        assert_eq!(
            engine.handle(token).expect_err("token").code,
            FailureCode::TokenProfileUnsupported
        );
    }

    #[test]
    fn request_validation_exercises_every_bounded_source_field() {
        fn rejected(candidate: &Request, expected: FailureCode) {
            let config = EngineConfig::local("/tmp/distill-policy-test/store.sqlite".into());
            assert_eq!(
                request_policy::validate(candidate, &config)
                    .expect_err("invalid request")
                    .code,
                expected
            );
        }

        let mut candidate = request(b"x", 1);
        candidate.request_id.clear();
        rejected(&candidate, FailureCode::InvalidRequest);
        candidate.request_id = "x".repeat(129);
        rejected(&candidate, FailureCode::InvalidRequest);

        candidate = request(&vec![0; MAX_SOURCE_BYTES + 1], 1);
        rejected(&candidate, FailureCode::InputTooLarge);

        for (root_id, relative_path) in [
            (String::new(), ByteString::default()),
            ("x".repeat(129), ByteString::default()),
            ("workspace".to_owned(), ByteString(vec![b'x'; 4_097])),
        ] {
            candidate = request(b"x", 1);
            candidate.source = Source::File {
                root_id,
                relative_path,
                binary_policy: BinaryPolicy::Accept,
            };
            rejected(&candidate, FailureCode::InvalidRequest);
        }

        let process = |executable: ByteString,
                       argv: Vec<ByteString>,
                       cwd_root_id: String,
                       cwd_relative_path: ByteString| Source::Process {
            executable,
            argv,
            cwd_root_id,
            cwd_relative_path,
            timeout_ms: None,
            environment_profile: None,
        };
        let process_cases = [
            (
                process(
                    ByteString::default(),
                    Vec::new(),
                    "workspace".to_owned(),
                    ByteString::default(),
                ),
                FailureCode::InvalidRequest,
            ),
            (
                process(
                    ByteString(vec![b'x'; 4_097]),
                    Vec::new(),
                    "workspace".to_owned(),
                    ByteString::default(),
                ),
                FailureCode::InvalidRequest,
            ),
            (
                process(
                    ByteString::from_utf8("/bin/echo"),
                    vec![ByteString::default(); 4_097],
                    "workspace".to_owned(),
                    ByteString::default(),
                ),
                FailureCode::ResourceExhausted,
            ),
            (
                process(
                    ByteString::from_utf8("/bin/echo"),
                    vec![ByteString(vec![b'x'; 1024 * 1024 + 1])],
                    "workspace".to_owned(),
                    ByteString::default(),
                ),
                FailureCode::ResourceExhausted,
            ),
            (
                process(
                    ByteString::from_utf8("/bin/echo"),
                    Vec::new(),
                    String::new(),
                    ByteString::default(),
                ),
                FailureCode::InvalidRequest,
            ),
            (
                process(
                    ByteString::from_utf8("/bin/echo"),
                    Vec::new(),
                    "x".repeat(129),
                    ByteString::default(),
                ),
                FailureCode::InvalidRequest,
            ),
            (
                process(
                    ByteString::from_utf8("/bin/echo"),
                    Vec::new(),
                    "workspace".to_owned(),
                    ByteString(vec![b'x'; 4_097]),
                ),
                FailureCode::InvalidRequest,
            ),
        ];
        for (source, expected) in process_cases {
            candidate = request(b"x", 1);
            candidate.source = source;
            rejected(&candidate, expected);
        }

        let valid_artifact = ArtifactRef {
            schema_version: ARTIFACT_SCHEMA_VERSION.to_owned(),
            id: "a".repeat(32),
            source_sha256: "b".repeat(64),
            source_bytes: 1,
            created_at: 1,
            expires_at: 2,
        };
        for artifact in [
            ArtifactRef {
                id: "a".repeat(31),
                ..valid_artifact.clone()
            },
            ArtifactRef {
                id: "A".repeat(32),
                ..valid_artifact.clone()
            },
            ArtifactRef {
                source_sha256: "b".repeat(63),
                ..valid_artifact.clone()
            },
            ArtifactRef {
                source_sha256: "G".repeat(64),
                ..valid_artifact
            },
        ] {
            candidate = request(b"x", 1);
            candidate.source = Source::Artifact { artifact };
            rejected(&candidate, FailureCode::InvalidRequest);
        }
    }

    #[test]
    fn process_argument_policy_accepts_exact_limit_and_rejects_one_byte_over() {
        let executable = ByteString::from_utf8("/bin/echo");
        let remaining = MAX_PROCESS_ARGUMENT_BYTES - executable.0.len();
        let width = remaining / MAX_PROCESS_ARGUMENTS;
        let remainder = remaining % MAX_PROCESS_ARGUMENTS;
        let mut argv = (0..MAX_PROCESS_ARGUMENTS)
            .map(|index| ByteString(vec![b'x'; width + usize::from(index < remainder)]))
            .collect::<Vec<_>>();

        request_policy::validate_process(&executable, &argv, Some(MIN_PROCESS_TIMEOUT_MS))
            .expect("exact aggregate limit");
        argv.last_mut().expect("last argument").0.push(b'x');
        assert_eq!(
            request_policy::validate_process(&executable, &argv, Some(MIN_PROCESS_TIMEOUT_MS),)
                .expect_err("one byte over")
                .code,
            FailureCode::ResourceExhausted
        );
    }

    #[test]
    fn over_limit_process_is_rejected_before_spawn_or_store_mutation() {
        let directory = tempfile::tempdir().expect("temp directory");
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let marker = workspace.join("must-not-exist");
        let shell = ["/bin/sh", "/usr/bin/sh"]
            .into_iter()
            .find(|path| std::path::Path::new(path).exists())
            .expect("shell");
        let script = "touch \"$0\"";
        let fixed_bytes = shell.len() + 2 + script.len() + marker.as_os_str().len();
        let padding = "x".repeat(MAX_PROCESS_ARGUMENT_BYTES + 1 - fixed_bytes);
        let mut config = EngineConfig::local(directory.path().join("private/store.sqlite"));
        config.roots.insert("workspace".to_owned(), workspace);
        let engine = Engine::new(config).expect("engine");
        let mut candidate = request(b"unused", 64);
        candidate.request_id = "over-limit-process".to_owned();
        candidate.source = Source::Process {
            executable: ByteString::from_utf8(shell),
            argv: vec![
                ByteString::from_utf8("-c"),
                ByteString::from_utf8(script),
                ByteString::from_utf8(marker.to_string_lossy()),
                ByteString::from_utf8(padding),
            ],
            cwd_root_id: "workspace".to_owned(),
            cwd_relative_path: ByteString::default(),
            timeout_ms: Some(MIN_PROCESS_TIMEOUT_MS),
            environment_profile: None,
        };

        let failure = engine.handle(candidate).expect_err("over-limit process");
        assert_eq!(failure.code, FailureCode::ResourceExhausted);
        assert_eq!(failure.request_id.as_deref(), Some("over-limit-process"));
        assert!(!marker.exists());
        assert_eq!(engine.status().expect("status").store_records, 0);
    }

    #[test]
    fn public_types_round_trip_without_information_loss() {
        let request = request(&[0, 1, 2, 255], 100);
        let json = serde_json::to_string(&request).expect("serialize");
        let decoded: Request = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, request);

        let outcome = Outcome {
            visible: VisiblePayload {
                bytes: "ok".to_owned(),
                media_type: "text/plain".to_owned(),
            },
            artifact: ArtifactRef {
                schema_version: ARTIFACT_SCHEMA_VERSION.to_owned(),
                id: "0".repeat(32),
                source_sha256: "a".repeat(64),
                source_bytes: 2,
                created_at: 1,
                expires_at: 2,
            },
            receipt: Receipt {
                schema_version: RECEIPT_SCHEMA_VERSION.to_owned(),
                request_id: "id".to_owned(),
                source_sha256: "a".repeat(64),
                artifact: ArtifactRef {
                    schema_version: ARTIFACT_SCHEMA_VERSION.to_owned(),
                    id: "0".repeat(32),
                    source_sha256: "a".repeat(64),
                    source_bytes: 2,
                    created_at: 1,
                    expires_at: 2,
                },
                projection_version: PROJECTION_VERSION.to_owned(),
                policy_version: POLICY_VERSION.to_owned(),
                token_profile: None,
                original_count: 2,
                visible_count: 2,
                count_unit: CountUnit::Bytes,
                fidelity: Fidelity::Exact,
                retained_spans: vec![ByteSpan { start: 0, end: 2 }],
                omitted_spans: Vec::new(),
                preservation: PreservationResult {
                    profile: "plain-text/v1".to_owned(),
                    mandatory_fact_ids: Vec::new(),
                },
                acquisition: AcquisitionReceipt {
                    variant: SourceVariant::Inline,
                    complete: true,
                    partial: false,
                    truncated: false,
                    root_id: None,
                    relative_path: None,
                    process: None,
                },
            },
        };
        let json = serde_json::to_string(&outcome).expect("serialize outcome");
        assert_eq!(
            serde_json::from_str::<Outcome>(&json).expect("deserialize outcome"),
            outcome
        );
    }

    #[test]
    fn receipt_fields_are_stable_across_one_hundred_repetitions() {
        let (_directory, engine) = fixture();
        let mut candidate = request(
            b"head\nnoise noise noise\nERROR E1: critical\nwarning W1: useful\ntail\n",
            48,
        );
        candidate.preservation_profile = "build-log/v1".to_owned();
        let first = engine.handle(candidate.clone()).expect("first");
        for _ in 0..99 {
            let repeated = engine.handle(candidate.clone()).expect("repeat");
            assert_eq!(repeated.visible, first.visible);
            assert_eq!(repeated.receipt.source_sha256, first.receipt.source_sha256);
            assert_eq!(
                repeated.receipt.projection_version,
                first.receipt.projection_version
            );
            assert_eq!(
                repeated.receipt.policy_version,
                first.receipt.policy_version
            );
            assert_eq!(
                repeated.receipt.original_count,
                first.receipt.original_count
            );
            assert_eq!(repeated.receipt.visible_count, first.receipt.visible_count);
            assert_eq!(repeated.receipt.fidelity, first.receipt.fidelity);
            assert_eq!(
                repeated.receipt.retained_spans,
                first.receipt.retained_spans
            );
            assert_eq!(repeated.receipt.omitted_spans, first.receipt.omitted_spans);
            assert_eq!(repeated.receipt.preservation, first.receipt.preservation);
        }
    }

    #[test]
    fn retention_resolution_and_configuration_are_strict() {
        assert_eq!(
            request_policy::resolve_expiration(
                &Retention {
                    expires_at: Some(20),
                    ttl_seconds: Some(10)
                },
                10,
                100
            )
            .expect_err("ambiguous")
            .code,
            FailureCode::InvalidRequest
        );
        assert_eq!(
            request_policy::resolve_expiration(
                &Retention {
                    expires_at: Some(10),
                    ttl_seconds: None
                },
                10,
                100
            )
            .expect_err("stale")
            .code,
            FailureCode::InvalidRequest
        );
        assert_eq!(
            request_policy::resolve_expiration(&Retention::default(), 10, 100).expect("default"),
            110
        );
        assert_eq!(
            request_policy::resolve_expiration(
                &Retention {
                    expires_at: None,
                    ttl_seconds: Some(1)
                },
                u64::MAX,
                100
            )
            .expect_err("ttl overflow")
            .code,
            FailureCode::InvalidRequest
        );
        assert_eq!(
            request_policy::resolve_expiration(&Retention::default(), u64::MAX, 1)
                .expect_err("default overflow")
                .code,
            FailureCode::InvalidRequest
        );

        let directory = tempfile::tempdir().expect("temp directory");
        let mut invalid = EngineConfig::local(directory.path().join("store.sqlite"));
        invalid.max_store_bytes = 0;
        assert_eq!(
            Engine::fixture(invalid, 1).err().expect("cap").code,
            FailureCode::InvalidRequest
        );
        let mut invalid = EngineConfig::local(directory.path().join("lineage-zero.sqlite"));
        invalid.max_lineage_bytes = 0;
        assert_eq!(
            Engine::fixture(invalid, 1)
                .err()
                .expect("zero lineage cap")
                .code,
            FailureCode::InvalidRequest
        );
        let mut invalid = EngineConfig::local(directory.path().join("lineage-high.sqlite"));
        invalid.max_lineage_bytes = MAX_LINEAGE_BYTES + 1;
        assert_eq!(
            Engine::fixture(invalid, 1)
                .err()
                .expect("high lineage cap")
                .code,
            FailureCode::InvalidRequest
        );

        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let mut overlapping = EngineConfig::local(workspace.join("store.sqlite"));
        overlapping.roots.insert("workspace".to_owned(), workspace);
        assert_eq!(
            Engine::fixture(overlapping, 1)
                .err()
                .expect("overlapping store")
                .code,
            FailureCode::UnsafeRoot
        );

        let mut invalid = EngineConfig::local(directory.path().join("other/store.sqlite"));
        invalid.busy_timeout_ms = 0;
        assert_eq!(
            Engine::fixture(invalid, 1)
                .err()
                .expect("busy timeout")
                .code,
            FailureCode::InvalidRequest
        );
        let mut invalid = EngineConfig::local(directory.path().join("other/store.sqlite"));
        invalid.default_ttl_seconds = 0;
        assert_eq!(
            Engine::fixture(invalid, 1).err().expect("retention").code,
            FailureCode::InvalidRequest
        );
        assert_eq!(
            Engine::fixture(EngineConfig::local("relative.sqlite".into()), 1)
                .err()
                .expect("relative store")
                .code,
            FailureCode::UnsafeRoot
        );
    }

    #[test]
    fn partial_process_capture_is_committed_but_never_projected_as_complete() {
        let directory = tempfile::tempdir().expect("temp directory");
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let mut config = EngineConfig::local(directory.path().join("private/store.sqlite"));
        config.roots.insert("workspace".to_owned(), workspace);
        let engine = Engine::new(config).expect("engine");
        let process = Request {
            contract_version: CONTRACT_VERSION.to_owned(),
            request_id: "timeout".to_owned(),
            source: Source::Process {
                executable: ByteString::from_utf8(
                    ["/usr/bin/sleep", "/bin/sleep"]
                        .into_iter()
                        .find(|path| std::path::Path::new(path).exists())
                        .expect("sleep"),
                ),
                argv: vec![ByteString::from_utf8("5")],
                cwd_root_id: "workspace".to_owned(),
                cwd_relative_path: ByteString::default(),
                timeout_ms: Some(100),
                environment_profile: None,
            },
            budget: Budget {
                unit: CountUnit::Bytes,
                total_visible_limit: 64,
                reserved_envelope: 0,
                token_profile: None,
            },
            preservation_profile: "plain-text/v1".to_owned(),
            retention: Retention::default(),
        };
        let failure = engine.handle(process).expect_err("timeout");
        assert_eq!(failure.code, FailureCode::AcquisitionFailed);
        let acquisition = failure.acquisition.expect("partial metadata");
        assert!(acquisition.partial);
        assert!(acquisition.process.expect("process").timed_out);
        let artifact = failure.artifact.expect("partial artifact");

        let mut recovery = request(b"unused", 64);
        recovery.request_id = "recovery".to_owned();
        recovery.source = Source::Artifact {
            artifact: artifact.clone(),
        };
        let rejected = engine
            .handle(recovery)
            .expect_err("partial artifacts are diagnosis-only");
        assert_eq!(rejected.code, FailureCode::AcquisitionFailed);
        assert_eq!(rejected.artifact, Some(artifact));
        assert!(rejected.acquisition.expect("partial metadata").partial);
    }

    #[test]
    fn clean_acquisition_failures_include_safe_deterministic_metadata() {
        let directory = tempfile::tempdir().expect("temp directory");
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        std::fs::write(workspace.join("binary"), [0, 0xff]).expect("binary file");
        let mut config = EngineConfig::local(directory.path().join("private/store.sqlite"));
        config.roots.insert("workspace".to_owned(), workspace);
        let engine = Engine::new(config).expect("engine");

        let mut binary = request(b"unused", 64);
        binary.source = Source::File {
            root_id: "workspace".to_owned(),
            relative_path: ByteString::from_utf8("binary"),
            binary_policy: BinaryPolicy::Reject,
        };
        let binary_failure = engine.handle(binary).expect_err("binary policy");
        assert_eq!(binary_failure.code, FailureCode::AcquisitionFailed);
        let binary_receipt = binary_failure.acquisition.expect("file metadata");
        assert_eq!(binary_receipt.variant, SourceVariant::File);
        assert!(!binary_receipt.complete);
        assert!(
            !binary_receipt
                .relative_path
                .expect("safe path")
                .contains("binary")
        );

        let mut spawn = request(b"unused", 64);
        spawn.source = Source::Process {
            executable: ByteString::from_utf8("/definitely/missing/distill-command"),
            argv: vec![ByteString::from_utf8("$(not-shell)")],
            cwd_root_id: "workspace".to_owned(),
            cwd_relative_path: ByteString::default(),
            timeout_ms: None,
            environment_profile: None,
        };
        let spawn_failure = engine.handle(spawn).expect_err("spawn");
        assert_eq!(spawn_failure.code, FailureCode::AcquisitionFailed);
        let spawn_receipt = spawn_failure.acquisition.expect("process metadata");
        assert_eq!(spawn_receipt.variant, SourceVariant::Process);
        assert!(!spawn_receipt.complete);
        assert_eq!(
            spawn_receipt.process.expect("process").working_directory,
            "workspace:<0 path bytes>"
        );
    }

    #[test]
    fn failures_and_receipts_never_copy_raw_source() {
        let (_directory, engine) = fixture();
        let secret = "DISTILL_TEST_SECRET_5f19";
        let mut candidate = request(format!("ERROR {secret}: mandatory\n").as_bytes(), 1);
        candidate.preservation_profile = "build-log/v1".to_owned();
        let failure = engine.handle(candidate).expect_err("budget failure");
        let encoded = serde_json::to_string(&failure).expect("failure JSON");
        assert!(!encoded.contains(secret));
        assert!(!failure.safe_message.contains(secret));
    }
}
