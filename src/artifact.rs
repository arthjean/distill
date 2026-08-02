use crate::contract::MAX_RECEIPT_SPANS;
#[cfg(test)]
use crate::types::MAX_ARTIFACT_LINEAGE_BYTES;
use crate::types::{
    ARTIFACT_SCHEMA_VERSION, AcquisitionReceipt, ArtifactRef, Failure, FailureCode, Receipt,
    ValidatedAcquisition,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

mod connection;
mod lifecycle;
mod lineage;
mod migration;
mod permissions;
mod receipt;
mod sqlite_errors;

use lifecycle::*;
use lineage::*;
use permissions::enforce_store_modes;
#[cfg(test)]
use permissions::sidecar_path;
use sqlite_errors::{map_read_error, map_write_error};

const STORE_SCHEMA_VERSION: i64 = 3;
const TOMBSTONE_TTL_SECONDS: u64 = 30 * 24 * 60 * 60;
const EMPTY_LINEAGE_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const LINEAGE_DIGEST_DOMAIN: &[u8] = b"distill.lineage/v1\0";

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

struct ArtifactRow {
    schema: String,
    state: String,
    bytes: Vec<u8>,
    digest: String,
    source_bytes: i64,
    acquisition_json: Vec<u8>,
    acquisition_digest: String,
    created_at: i64,
    expires_at: i64,
}

struct VerifiedArtifact {
    reference: ArtifactRef,
    stored: StoredArtifact,
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

    fn missing_artifact_failure(
        connection: &Connection,
        id: &str,
        now: u64,
    ) -> Result<Failure, Failure> {
        let tombstone = connection
            .query_row(
                "SELECT purge_at FROM tombstones WHERE id = ?1",
                [id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(map_read_error)?;
        if tombstone.is_some_and(|purge_at| purge_at >= 0 && now < purge_at as u64) {
            return Ok(Failure::new(
                FailureCode::ArtifactExpired,
                "artifact retention expired",
            ));
        }
        Ok(Failure::new(
            FailureCode::ArtifactUnknown,
            "artifact does not exist",
        ))
    }
}

fn read_artifact(
    connection: &Connection,
    reference: &ArtifactRef,
    now: u64,
) -> Result<StoredArtifact, Failure> {
    if reference.schema_version != ARTIFACT_SCHEMA_VERSION {
        return Err(Failure::new(
            FailureCode::ArtifactSchemaUnsupported,
            "artifact reference schema is unsupported",
        ));
    }
    let verified = load_verified_artifact(connection, &reference.id, now)?;
    if verified.reference != *reference {
        return Err(Failure::new(
            FailureCode::ArtifactCorrupt,
            "artifact integrity verification failed",
        )
        .with_artifact(verified.reference));
    }
    Ok(verified.stored)
}

fn load_verified_artifact(
    connection: &Connection,
    id: &str,
    now: u64,
) -> Result<VerifiedArtifact, Failure> {
    let record: Option<ArtifactRow> = connection
        .query_row(
            "SELECT schema_version, state, source, sha256, source_bytes,
                    source_metadata, source_metadata_sha256, created_at, expires_at
             FROM artifacts WHERE id = ?1",
            [id],
            |row| {
                Ok(ArtifactRow {
                    schema: row.get(0)?,
                    state: row.get(1)?,
                    bytes: row.get(2)?,
                    digest: row.get(3)?,
                    source_bytes: row.get(4)?,
                    acquisition_json: row.get(5)?,
                    acquisition_digest: row.get(6)?,
                    created_at: row.get(7)?,
                    expires_at: row.get(8)?,
                })
            },
        )
        .optional()
        .map_err(map_read_error)?;
    let Some(row) = record else {
        return Err(ArtifactStore::missing_artifact_failure(
            connection, id, now,
        )?);
    };
    if row.schema != ARTIFACT_SCHEMA_VERSION {
        return Err(Failure::new(
            FailureCode::ArtifactSchemaUnsupported,
            "stored artifact schema is unsupported",
        ));
    }
    if row.state != "committed" {
        return Err(Failure::new(
            FailureCode::CommitFailed,
            "artifact transaction did not reach committed state",
        ));
    }
    let source_bytes = nonnegative_u64(row.source_bytes, "source byte count")?;
    let created_at = nonnegative_u64(row.created_at, "creation time")?;
    let expires_at = nonnegative_u64(row.expires_at, "expiration time")?;
    let stored_reference = ArtifactRef {
        schema_version: row.schema,
        id: id.to_owned(),
        source_sha256: row.digest.clone(),
        source_bytes,
        created_at,
        expires_at,
    };
    if now >= expires_at {
        return Err(
            Failure::new(FailureCode::ArtifactExpired, "artifact retention expired")
                .with_artifact(stored_reference),
        );
    }
    if row.bytes.len() as u64 != source_bytes || sha256_hex(&row.bytes) != row.digest {
        return Err(Failure::new(
            FailureCode::ArtifactCorrupt,
            "artifact integrity verification failed",
        )
        .with_artifact(stored_reference));
    }
    verify_exact_digest(
        &row.acquisition_json,
        &row.acquisition_digest,
        "artifact acquisition metadata integrity verification failed",
        &stored_reference,
    )?;
    let acquisition: AcquisitionReceipt =
        serde_json::from_slice(&row.acquisition_json).map_err(|_| {
            Failure::new(
                FailureCode::ArtifactCorrupt,
                "artifact acquisition metadata is corrupt",
            )
            .with_artifact(stored_reference.clone())
        })?;
    let acquisition = ValidatedAcquisition::from_wire(acquisition, source_bytes).map_err(|_| {
        Failure::new(
            FailureCode::ArtifactCorrupt,
            "artifact acquisition metadata is semantically corrupt",
        )
        .with_artifact(stored_reference.clone())
    })?;
    Ok(VerifiedArtifact {
        reference: stored_reference,
        stored: StoredArtifact {
            bytes: row.bytes,
            acquisition,
        },
    })
}

fn nonnegative_u64(value: i64, label: &str) -> Result<u64, Failure> {
    u64::try_from(value).map_err(|_| {
        Failure::new(
            FailureCode::ArtifactCorrupt,
            format!("artifact {label} is invalid"),
        )
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
#[path = "artifact/tests.rs"]
mod tests;
