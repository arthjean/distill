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

pub use contract::{
    DEFAULT_SELECTOR_CONTEXT_LINES, DEFAULT_SELECTOR_MATCHES, MAX_IDENTIFIER_BYTES, MAX_PATH_BYTES,
    MAX_SELECTOR_CONTEXT_LINES, MAX_SELECTOR_MATCHES, MAX_SELECTOR_PATTERN_BYTES,
};
pub use request_policy::{
    MAX_PROCESS_ARGUMENT_BYTES, MAX_PROCESS_ARGUMENTS, MAX_PROCESS_EXECUTABLE_BYTES,
    MAX_PROCESS_TIMEOUT_MS, MAX_SOURCE_BYTES, MIN_PROCESS_TIMEOUT_MS, validate_artifact_selector,
};
pub use types::{
    ARTIFACT_SCHEMA_VERSION, AcquisitionReceipt, ArtifactRef, ArtifactSelector, ArtifactTrace,
    BinaryPolicy, Budget, ByteSpan, ByteString, CL100K_PROFILE, CONTRACT_VERSION,
    CONTRACT_VERSION_V2, CountUnit, EngineConfig, EngineStatus, Failure, FailureCode, Fidelity,
    GC_SCHEMA_VERSION, GarbageCollection, MAX_ARTIFACT_LINEAGE_BYTES, MAX_LINEAGE_BYTES, Outcome,
    POLICY_VERSION, PROJECTION_VERSION, PreservationResult, ProcessReceipt, ProcessStream,
    RECEIPT_SCHEMA_VERSION, RESTORE_SCHEMA_VERSION, Receipt, Request, RestoredArtifact, Retention,
    STATUS_SCHEMA_VERSION, ScalarValue, Source, SourceVariant, StreamEvent, TRACE_SCHEMA_VERSION,
    VisiblePayload, supported_contract_version,
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
            request_policy::ValidatedSource::Artifact(artifact, _) => {
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
                let receipt = ValidatedAcquisition::artifact_replay(stored.bytes.len() as u64);
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

        let projected = match &request.source {
            request_policy::ValidatedSource::Artifact(_, Some(selection)) => {
                projection::project_selection(&acquired.bytes, selection, projection_spec)
            }
            _ => projection::project_validated(&acquired.bytes, projection_spec),
        };
        let projection = projected.map_err(|failure| {
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
#[path = "lib/tests.rs"]
mod tests;
