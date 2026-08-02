#[cfg(test)]
use crate::types::MAX_ARTIFACT_LINEAGE_BYTES;
use crate::types::{
    ARTIFACT_SCHEMA_VERSION, AcquisitionReceipt, ArtifactRef, Failure, FailureCode, Receipt,
    ValidatedAcquisition,
};
#[cfg(test)]
use rusqlite::Connection;
use rusqlite::{TransactionBehavior, params};
use std::path::PathBuf;

mod connection;
mod integrity;
mod lifecycle;
mod lineage;
mod migration;
mod permissions;
mod receipt;
mod record;
mod sqlite_errors;

use integrity::{EMPTY_LINEAGE_SHA256, nonnegative_u64, sha256_hex};
use lifecycle::{collect_in_transaction, store_usage};
use lineage::lineage_usage_connection;
use permissions::enforce_store_modes;
#[cfg(test)]
use permissions::sidecar_path;
use record::{load_verified_artifact, read_artifact};
use sqlite_errors::{map_read_error, map_write_error};

const STORE_SCHEMA_VERSION: i64 = 3;
const TOMBSTONE_TTL_SECONDS: u64 = 30 * 24 * 60 * 60;
#[derive(Clone, Debug)]
pub(crate) struct ArtifactStore {
    path: PathBuf,
    max_bytes: u64,
    max_lineage_bytes: u64,
    busy_timeout_ms: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct StoredArtifact {
    pub bytes: Vec<u8>,
    pub acquisition: ValidatedAcquisition,
}

#[derive(Clone, Debug)]
pub(crate) struct StoredTrace {
    pub acquisition: AcquisitionReceipt,
    pub receipts: Vec<Receipt>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StoreReport {
    pub bytes: u64,
    pub lineage_bytes: u64,
    pub records: u64,
    pub expired_records: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GcReport {
    pub reclaimed_bytes: u64,
    pub reclaimed_lineage_bytes: u64,
    pub reclaimed_records: u64,
}

struct CommitReadbackRow {
    digest: String,
    bytes: Vec<u8>,
    acquisition_json: Vec<u8>,
    acquisition_digest: String,
    state: String,
}

struct StatusRow {
    records: i64,
    bytes: i64,
    expired: i64,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommitFault {
    BeforeInsert,
    BeforeCommit,
    AfterCommit,
    BeforeReadback,
}

impl ArtifactStore {
    pub(crate) fn new(
        path: PathBuf,
        max_bytes: u64,
        max_lineage_bytes: u64,
        busy_timeout_ms: u64,
    ) -> Self {
        Self {
            path,
            max_bytes,
            max_lineage_bytes,
            busy_timeout_ms,
        }
    }

    pub(crate) fn initialize(&self) -> Result<(), Failure> {
        let _connection = self.initialize_connection()?;
        Ok(())
    }

    #[cfg(not(test))]
    pub(crate) fn commit(
        &self,
        id: &str,
        bytes: &[u8],
        acquisition: &ValidatedAcquisition,
        created_at: u64,
        expires_at: u64,
    ) -> Result<ArtifactRef, Failure> {
        self.commit_inner(id, bytes, acquisition, created_at, expires_at)
    }

    #[cfg(test)]
    pub(crate) fn commit(
        &self,
        id: &str,
        bytes: &[u8],
        acquisition: &ValidatedAcquisition,
        created_at: u64,
        expires_at: u64,
    ) -> Result<ArtifactRef, Failure> {
        self.commit_inner(id, bytes, acquisition, created_at, expires_at, None)
    }

    #[cfg(test)]
    fn commit_unvalidated(
        &self,
        id: &str,
        bytes: &[u8],
        acquisition: &AcquisitionReceipt,
        created_at: u64,
        expires_at: u64,
        fault: Option<CommitFault>,
    ) -> Result<ArtifactRef, Failure> {
        let acquisition = ValidatedAcquisition::from_wire(acquisition.clone(), bytes.len() as u64)
            .map_err(|message| Failure::new(FailureCode::InvariantBreach, message))?;
        self.commit_inner(id, bytes, &acquisition, created_at, expires_at, fault)
    }

    fn commit_inner(
        &self,
        id: &str,
        bytes: &[u8],
        acquisition: &ValidatedAcquisition,
        created_at: u64,
        expires_at: u64,
        #[cfg(test)] fault: Option<CommitFault>,
    ) -> Result<ArtifactRef, Failure> {
        if id.len() != 32
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(Failure::new(
                FailureCode::InvariantBreach,
                "artifact ID generator returned an invalid value",
            ));
        }
        let source_bytes = u64::try_from(bytes.len()).map_err(|_| {
            Failure::new(
                FailureCode::ResourceExhausted,
                "source byte length is not representable",
            )
        })?;
        if acquisition.source_bytes() != source_bytes {
            return Err(Failure::new(
                FailureCode::InvariantBreach,
                "validated acquisition does not match the source byte count",
            ));
        }
        let source_bytes_sql = i64::try_from(source_bytes).map_err(|_| {
            Failure::new(
                FailureCode::ResourceExhausted,
                "source byte length exceeds SQLite limits",
            )
        })?;
        let created_at_sql = i64::try_from(created_at).map_err(|_| {
            Failure::new(
                FailureCode::InvalidRequest,
                "creation time exceeds SQLite limits",
            )
        })?;
        let expires_at_sql = i64::try_from(expires_at).map_err(|_| {
            Failure::new(
                FailureCode::InvalidRequest,
                "expiration time exceeds SQLite limits",
            )
        })?;
        if expires_at <= created_at {
            return Err(Failure::new(
                FailureCode::InvalidRequest,
                "artifact expiration must follow creation",
            ));
        }
        let digest = sha256_hex(bytes);
        let acquisition_json = serde_json::to_vec(&acquisition.to_receipt()).map_err(|_| {
            Failure::new(
                FailureCode::InvariantBreach,
                "acquisition metadata cannot be serialized",
            )
        })?;
        let acquisition_digest = sha256_hex(&acquisition_json);
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_write_error)?;
        let _gc = collect_in_transaction(&transaction, created_at)?;
        let used = store_usage(&transaction)?;
        if used.saturating_add(source_bytes) > self.max_bytes {
            return Err(Failure::new(
                FailureCode::StoreFull,
                "artifact store has no capacity without evicting live data",
            ));
        }
        #[cfg(test)]
        if fault == Some(CommitFault::BeforeInsert) {
            return Err(Failure::new(
                FailureCode::CommitFailed,
                "injected failure before artifact insertion",
            ));
        }
        transaction
            .execute(
                "INSERT INTO artifacts
                 (id, schema_version, state, source, sha256, source_bytes,
                  source_metadata, source_metadata_sha256, lineage_count,
                  lineage_bytes, lineage_head_sha256, created_at, expires_at)
                 VALUES (?1, ?2, 'committed', ?3, ?4, ?5, ?6, ?7, 0, 0,
                         ?8, ?9, ?10)",
                params![
                    id,
                    ARTIFACT_SCHEMA_VERSION,
                    bytes,
                    digest,
                    source_bytes_sql,
                    acquisition_json,
                    acquisition_digest,
                    EMPTY_LINEAGE_SHA256,
                    created_at_sql,
                    expires_at_sql,
                ],
            )
            .map_err(map_write_error)?;
        #[cfg(test)]
        if fault == Some(CommitFault::BeforeCommit) {
            return Err(Failure::new(
                FailureCode::CommitFailed,
                "injected failure before artifact commit",
            ));
        }
        transaction.commit().map_err(map_write_error)?;
        #[cfg(test)]
        if fault == Some(CommitFault::AfterCommit) {
            return Err(Failure::new(
                FailureCode::CommitFailed,
                "injected interruption after artifact commit",
            ));
        }
        enforce_store_modes(&self.path)?;
        #[cfg(test)]
        if fault == Some(CommitFault::BeforeReadback) {
            return Err(Failure::new(
                FailureCode::CommitFailed,
                "injected interruption before artifact readback",
            ));
        }
        let stored: CommitReadbackRow = connection
            .query_row(
                "SELECT sha256, source, source_metadata, source_metadata_sha256, state
                 FROM artifacts WHERE id = ?1",
                [id],
                |row| {
                    Ok(CommitReadbackRow {
                        digest: row.get(0)?,
                        bytes: row.get(1)?,
                        acquisition_json: row.get(2)?,
                        acquisition_digest: row.get(3)?,
                        state: row.get(4)?,
                    })
                },
            )
            .map_err(map_read_error)?;
        if stored.state != "committed"
            || stored.digest != digest
            || stored.bytes.len() != bytes.len()
            || sha256_hex(&stored.bytes) != digest
            || stored.acquisition_json != acquisition_json
            || stored.acquisition_digest != acquisition_digest
            || sha256_hex(&stored.acquisition_json) != acquisition_digest
        {
            return Err(Failure::new(
                FailureCode::CommitFailed,
                "committed artifact failed integrity readback",
            ));
        }
        Ok(ArtifactRef {
            schema_version: ARTIFACT_SCHEMA_VERSION.to_owned(),
            id: id.to_owned(),
            source_sha256: digest,
            source_bytes,
            created_at,
            expires_at,
        })
    }

    pub(crate) fn retrieve(
        &self,
        reference: &ArtifactRef,
        now: u64,
    ) -> Result<StoredArtifact, Failure> {
        let connection = self.connect()?;
        let result = read_artifact(&connection, reference, now);
        if result
            .as_ref()
            .is_err_and(|failure| failure.code == FailureCode::ArtifactExpired)
        {
            drop(connection);
            let _report = self.collect_garbage(now)?;
        }
        result
    }

    pub(crate) fn reference_by_id(&self, id: &str, now: u64) -> Result<ArtifactRef, Failure> {
        if id.len() != 32
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(Failure::new(
                FailureCode::InvalidRequest,
                "artifact ID must be 32 lowercase hexadecimal characters",
            ));
        }
        let connection = self.connect()?;
        let result = load_verified_artifact(&connection, id, now).map(|stored| stored.reference);
        if result
            .as_ref()
            .is_err_and(|failure| failure.code == FailureCode::ArtifactExpired)
        {
            drop(connection);
            let _report = self.collect_garbage(now)?;
        }
        result
    }

    pub(crate) fn status(&self, now: u64) -> Result<StoreReport, Failure> {
        let connection = self.connect()?;
        let now_sql = i64::try_from(now).map_err(|_| {
            Failure::new(
                FailureCode::InvalidRequest,
                "status time exceeds SQLite limits",
            )
        })?;
        let row: StatusRow = connection
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(source_bytes), 0),
                        COALESCE(SUM(CASE WHEN expires_at <= ?1 THEN 1 ELSE 0 END), 0)
                 FROM artifacts",
                [now_sql],
                |row| {
                    Ok(StatusRow {
                        records: row.get(0)?,
                        bytes: row.get(1)?,
                        expired: row.get(2)?,
                    })
                },
            )
            .map_err(map_read_error)?;
        Ok(StoreReport {
            bytes: nonnegative_u64(row.bytes, "store usage")?,
            lineage_bytes: lineage_usage_connection(&connection)?,
            records: nonnegative_u64(row.records, "store record count")?,
            expired_records: nonnegative_u64(row.expired, "expired record count")?,
        })
    }

    pub(crate) fn collect_garbage(&self, now: u64) -> Result<GcReport, Failure> {
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_write_error)?;
        let report = collect_in_transaction(&transaction, now)?;
        transaction.commit().map_err(map_write_error)?;
        enforce_store_modes(&self.path)?;
        Ok(report)
    }
}

#[cfg(test)]
#[path = "artifact/tests.rs"]
mod tests;
