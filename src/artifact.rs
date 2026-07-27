use crate::types::{
    ARTIFACT_SCHEMA_VERSION, AcquisitionReceipt, ArtifactRef, CL100K_PROFILE, CountUnit, Failure,
    FailureCode, Fidelity, MAX_ARTIFACT_LINEAGE_BYTES, POLICY_VERSION, PROJECTION_VERSION,
    RECEIPT_SCHEMA_VERSION, Receipt,
};
use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use sha2::{Digest, Sha256};
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

const STORE_SCHEMA_VERSION: i64 = 3;
const TOMBSTONE_TTL_SECONDS: u64 = 30 * 24 * 60 * 60;
const MAX_RECEIPT_SPANS: usize = 257;
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
    pub acquisition: AcquisitionReceipt,
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

type ReceiptTarget = (String, String, i64, i64, i64, i64, i64, String);

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
        let _connection = self.open(true)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn commit(
        &self,
        id: &str,
        bytes: &[u8],
        acquisition: &AcquisitionReceipt,
        created_at: u64,
        expires_at: u64,
        fault: Option<CommitFault>,
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
        acquisition
            .validate(source_bytes)
            .map_err(|message| Failure::new(FailureCode::InvariantBreach, message))?;
        let digest = sha256_hex(bytes);
        let acquisition_json = serde_json::to_vec(acquisition).map_err(|_| {
            Failure::new(
                FailureCode::InvariantBreach,
                "acquisition metadata cannot be serialized",
            )
        })?;
        let acquisition_digest = sha256_hex(&acquisition_json);
        let mut connection = self.open(false)?;
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
        if fault == Some(CommitFault::BeforeCommit) {
            return Err(Failure::new(
                FailureCode::CommitFailed,
                "injected failure before artifact commit",
            ));
        }
        transaction.commit().map_err(map_write_error)?;
        if fault == Some(CommitFault::AfterCommit) {
            return Err(Failure::new(
                FailureCode::CommitFailed,
                "injected interruption after artifact commit",
            ));
        }
        enforce_store_modes(&self.path)?;
        if fault == Some(CommitFault::BeforeReadback) {
            return Err(Failure::new(
                FailureCode::CommitFailed,
                "injected interruption before artifact readback",
            ));
        }
        let (stored_digest, stored_bytes, stored_acquisition, stored_acquisition_digest, state): (
            String,
            Vec<u8>,
            Vec<u8>,
            String,
            String,
        ) = connection
            .query_row(
                "SELECT sha256, source, source_metadata, source_metadata_sha256, state
                 FROM artifacts WHERE id = ?1",
                [id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .map_err(map_read_error)?;
        if state != "committed"
            || stored_digest != digest
            || stored_bytes.len() != bytes.len()
            || sha256_hex(&stored_bytes) != digest
            || stored_acquisition != acquisition_json
            || stored_acquisition_digest != acquisition_digest
            || sha256_hex(&stored_acquisition) != acquisition_digest
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
        let connection = self.open(false)?;
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
        let connection = self.open(false)?;
        let record = connection
            .query_row(
                "SELECT schema_version, sha256, source_bytes, created_at, expires_at
                 FROM artifacts WHERE id = ?1",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(map_read_error)?;
        let Some((schema_version, source_sha256, source_bytes, created_at, expires_at)) = record
        else {
            let tombstone = connection
                .query_row(
                    "SELECT purge_at FROM tombstones WHERE id = ?1",
                    [id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(map_read_error)?;
            return Err(
                if tombstone.is_some_and(|purge_at| purge_at >= 0 && now < purge_at as u64) {
                    Failure::new(FailureCode::ArtifactExpired, "artifact retention expired")
                } else {
                    Failure::new(FailureCode::ArtifactUnknown, "artifact does not exist")
                },
            );
        };
        if schema_version != ARTIFACT_SCHEMA_VERSION {
            return Err(Failure::new(
                FailureCode::ArtifactSchemaUnsupported,
                "stored artifact schema is unsupported",
            ));
        }
        let reference = ArtifactRef {
            schema_version,
            id: id.to_owned(),
            source_sha256,
            source_bytes: nonnegative_u64(source_bytes, "source byte count")?,
            created_at: nonnegative_u64(created_at, "creation time")?,
            expires_at: nonnegative_u64(expires_at, "expiration time")?,
        };
        if now >= reference.expires_at {
            drop(connection);
            let _report = self.collect_garbage(now)?;
            return Err(
                Failure::new(FailureCode::ArtifactExpired, "artifact retention expired")
                    .with_artifact(reference),
            );
        }
        Ok(reference)
    }

    pub(crate) fn record_receipt(
        &self,
        reference: &ArtifactRef,
        receipt: &Receipt,
    ) -> Result<(), Failure> {
        validate_receipt(receipt, reference, FailureCode::InvariantBreach)?;
        let receipt_json = serde_json::to_vec(receipt).map_err(|_| {
            Failure::new(
                FailureCode::InvariantBreach,
                "projection receipt cannot be serialized",
            )
        })?;
        let receipt_bytes = u64::try_from(receipt_json.len()).map_err(|_| {
            Failure::new(
                FailureCode::ResourceExhausted,
                "projection receipt size is not representable",
            )
        })?;
        if receipt_bytes > MAX_ARTIFACT_LINEAGE_BYTES {
            return Err(Failure::new(
                FailureCode::ResourceExhausted,
                "projection receipt exceeds the per-artifact lineage cap",
            ));
        }
        let receipt_digest = sha256_hex(&receipt_json);
        let mut connection = self.open(false)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_write_error)?;
        let target: Option<ReceiptTarget> = transaction
            .query_row(
                "SELECT schema_version, sha256, source_bytes, created_at, expires_at,
                        lineage_count, lineage_bytes, lineage_head_sha256
                 FROM artifacts WHERE id = ?1 AND state = 'committed'",
                [&reference.id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .optional()
            .map_err(map_read_error)?;
        let Some((
            schema,
            digest,
            source_bytes,
            created_at,
            expires_at,
            claimed_count,
            claimed_bytes,
            claimed_head,
        )) = target
        else {
            return Err(Failure::new(
                FailureCode::ArtifactCorrupt,
                "artifact receipt target failed integrity validation",
            )
            .with_artifact(reference.clone()));
        };
        if schema != reference.schema_version
            || digest != reference.source_sha256
            || nonnegative_u64(source_bytes, "source byte count")? != reference.source_bytes
            || nonnegative_u64(created_at, "creation time")? != reference.created_at
            || nonnegative_u64(expires_at, "expiration time")? != reference.expires_at
        {
            return Err(Failure::new(
                FailureCode::ArtifactCorrupt,
                "artifact receipt target failed integrity validation",
            )
            .with_artifact(reference.clone()));
        }
        let global_lineage = lineage_usage(&transaction)?;
        let shape = lineage_shape(&transaction, reference)?;
        let claimed_count = nonnegative_u64(claimed_count, "claimed receipt count")?;
        let claimed_bytes = nonnegative_u64(claimed_bytes, "claimed lineage usage")?;
        if shape.count != claimed_count
            || shape.bytes != claimed_bytes
            || !valid_sha256(&claimed_head)
            || (shape.count == 0 && claimed_head != EMPTY_LINEAGE_SHA256)
            || (shape.count > 0 && shape.last_chain.as_deref() != Some(claimed_head.as_str()))
        {
            return Err(Failure::new(
                FailureCode::ArtifactCorrupt,
                "artifact receipt lineage commitment is corrupt",
            )
            .with_artifact(reference.clone()));
        }
        let next_global = global_lineage.checked_add(receipt_bytes);
        let next_artifact = shape.bytes.checked_add(receipt_bytes);
        if next_global.is_none_or(|bytes| bytes > self.max_lineage_bytes)
            || next_artifact.is_none_or(|bytes| bytes > MAX_ARTIFACT_LINEAGE_BYTES)
        {
            return Err(Failure::new(
                FailureCode::ResourceExhausted,
                "artifact receipt lineage capacity is exhausted",
            ));
        }
        let next_artifact = next_artifact.ok_or_else(|| {
            Failure::new(
                FailureCode::ResourceExhausted,
                "artifact receipt lineage capacity is exhausted",
            )
        })?;
        let lineage_sequence = i64::try_from(shape.count).map_err(|_| {
            Failure::new(
                FailureCode::ResourceExhausted,
                "artifact receipt sequence is exhausted",
            )
        })?;
        let lineage_chain = lineage_chain_sha256(&claimed_head, shape.count, &receipt_json);
        let changed = transaction
            .execute(
                "INSERT INTO artifact_receipts
                    (artifact_id, lineage_sequence, request_id, receipt_metadata,
                     receipt_metadata_sha256, lineage_chain_sha256)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    reference.id,
                    lineage_sequence,
                    receipt.request_id,
                    receipt_json,
                    receipt_digest,
                    lineage_chain,
                ],
            )
            .map_err(map_write_error)?;
        if changed != 1 {
            return Err(Failure::new(
                FailureCode::ArtifactCorrupt,
                "artifact receipt target failed integrity validation",
            )
            .with_artifact(reference.clone()));
        }
        let next_count = shape.count.checked_add(1).ok_or_else(|| {
            Failure::new(
                FailureCode::ResourceExhausted,
                "artifact receipt sequence is exhausted",
            )
        })?;
        let updated = transaction
            .execute(
                "UPDATE artifacts
                 SET lineage_count = ?1, lineage_bytes = ?2, lineage_head_sha256 = ?3
                 WHERE id = ?4",
                params![
                    i64::try_from(next_count).map_err(|_| {
                        Failure::new(
                            FailureCode::ResourceExhausted,
                            "artifact receipt sequence is exhausted",
                        )
                    })?,
                    i64::try_from(next_artifact).map_err(|_| {
                        Failure::new(
                            FailureCode::ResourceExhausted,
                            "artifact lineage usage is not representable",
                        )
                    })?,
                    lineage_chain,
                    reference.id,
                ],
            )
            .map_err(map_write_error)?;
        if updated != 1 {
            return Err(Failure::new(
                FailureCode::ArtifactCorrupt,
                "artifact lineage commitment could not be updated",
            )
            .with_artifact(reference.clone()));
        }
        transaction.commit().map_err(map_write_error)?;
        enforce_store_modes(&self.path)?;
        Ok(())
    }

    pub(crate) fn trace(&self, reference: &ArtifactRef, now: u64) -> Result<StoredTrace, Failure> {
        let mut connection = self.open(false)?;
        let transaction = connection.transaction().map_err(map_read_error)?;
        let stored = read_artifact(&transaction, reference, now)?;
        let (claimed_count, claimed_bytes, claimed_head): (i64, i64, String) = transaction
            .query_row(
                "SELECT lineage_count, lineage_bytes, lineage_head_sha256
                 FROM artifacts WHERE id = ?1",
                [&reference.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(map_read_error)?;
        let claimed_count = nonnegative_u64(claimed_count, "claimed receipt count")?;
        let claimed_bytes = nonnegative_u64(claimed_bytes, "claimed lineage usage")?;
        let shape = lineage_shape(&transaction, reference)?;
        if shape.count != claimed_count
            || shape.bytes != claimed_bytes
            || shape.bytes > MAX_ARTIFACT_LINEAGE_BYTES
            || !valid_sha256(&claimed_head)
            || (shape.count == 0 && claimed_head != EMPTY_LINEAGE_SHA256)
            || (shape.count > 0 && shape.last_chain.as_deref() != Some(claimed_head.as_str()))
        {
            return Err(Failure::new(
                FailureCode::ArtifactCorrupt,
                "artifact receipt lineage commitment is corrupt",
            )
            .with_artifact(reference.clone()));
        }
        let mut statement = transaction
            .prepare(
                "SELECT lineage_sequence, request_id, receipt_metadata,
                        receipt_metadata_sha256, lineage_chain_sha256
                 FROM artifact_receipts
                 WHERE artifact_id = ?1 ORDER BY lineage_sequence, sequence",
            )
            .map_err(map_read_error)?;
        let mut rows = statement.query([&reference.id]).map_err(map_read_error)?;
        let mut receipts = Vec::new();
        let mut expected_sequence = 0_u64;
        let mut lineage_bytes = 0_u64;
        let mut lineage_head = EMPTY_LINEAGE_SHA256.to_owned();
        while let Some(row) = rows.next().map_err(map_read_error)? {
            let sequence = nonnegative_u64(
                row.get::<_, i64>(0).map_err(map_read_error)?,
                "receipt lineage sequence",
            )?;
            if sequence != expected_sequence {
                return Err(Failure::new(
                    FailureCode::ArtifactCorrupt,
                    "artifact receipt lineage sequence is corrupt",
                )
                .with_artifact(reference.clone()));
            }
            expected_sequence = expected_sequence.saturating_add(1);
            let request_id = row.get::<_, String>(1).map_err(map_read_error)?;
            let receipt_json = row.get::<_, Vec<u8>>(2).map_err(map_read_error)?;
            let receipt_digest = row.get::<_, String>(3).map_err(map_read_error)?;
            let stored_chain = row.get::<_, String>(4).map_err(map_read_error)?;
            verify_exact_digest(
                &receipt_json,
                &receipt_digest,
                "artifact projection receipt integrity verification failed",
                reference,
            )?;
            lineage_bytes = lineage_bytes.saturating_add(receipt_json.len() as u64);
            if lineage_bytes > MAX_ARTIFACT_LINEAGE_BYTES {
                return Err(Failure::new(
                    FailureCode::ArtifactCorrupt,
                    "artifact receipt lineage exceeds its logical cap",
                )
                .with_artifact(reference.clone()));
            }
            let expected_chain = lineage_chain_sha256(&lineage_head, sequence, &receipt_json);
            if stored_chain != expected_chain {
                return Err(Failure::new(
                    FailureCode::ArtifactCorrupt,
                    "artifact receipt lineage commitment is corrupt",
                )
                .with_artifact(reference.clone()));
            }
            lineage_head = expected_chain;
            let receipt: Receipt = serde_json::from_slice(&receipt_json).map_err(|_| {
                Failure::new(
                    FailureCode::ArtifactCorrupt,
                    "artifact projection receipt is corrupt",
                )
                .with_artifact(reference.clone())
            })?;
            if receipt.request_id != request_id {
                return Err(Failure::new(
                    FailureCode::ArtifactCorrupt,
                    "artifact projection receipt lineage is inconsistent",
                )
                .with_artifact(reference.clone()));
            }
            validate_receipt(&receipt, reference, FailureCode::ArtifactCorrupt)?;
            receipts.push(receipt);
        }
        drop(rows);
        drop(statement);
        if lineage_bytes != claimed_bytes
            || expected_sequence != claimed_count
            || lineage_head != claimed_head
        {
            return Err(Failure::new(
                FailureCode::ArtifactCorrupt,
                "artifact receipt lineage commitment is corrupt",
            )
            .with_artifact(reference.clone()));
        }
        transaction.commit().map_err(map_read_error)?;
        Ok(StoredTrace {
            acquisition: stored.acquisition,
            receipts,
        })
    }

    pub(crate) fn status(&self, now: u64) -> Result<StoreReport, Failure> {
        let connection = self.open(false)?;
        let now_sql = i64::try_from(now).map_err(|_| {
            Failure::new(
                FailureCode::InvalidRequest,
                "status time exceeds SQLite limits",
            )
        })?;
        let (records, bytes, expired): (i64, i64, i64) = connection
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(source_bytes), 0),
                        COALESCE(SUM(CASE WHEN expires_at <= ?1 THEN 1 ELSE 0 END), 0)
                 FROM artifacts",
                [now_sql],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(map_read_error)?;
        Ok(StoreReport {
            bytes: nonnegative_u64(bytes, "store usage")?,
            lineage_bytes: lineage_usage_connection(&connection)?,
            records: nonnegative_u64(records, "store record count")?,
            expired_records: nonnegative_u64(expired, "expired record count")?,
        })
    }

    pub(crate) fn collect_garbage(&self, now: u64) -> Result<GcReport, Failure> {
        let mut connection = self.open(false)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_write_error)?;
        let report = collect_in_transaction(&transaction, now)?;
        transaction.commit().map_err(map_write_error)?;
        enforce_store_modes(&self.path)?;
        Ok(report)
    }

    fn missing_artifact(
        connection: &Connection,
        id: &str,
        now: u64,
    ) -> Result<StoredArtifact, Failure> {
        let tombstone = connection
            .query_row(
                "SELECT purge_at FROM tombstones WHERE id = ?1",
                [id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(map_read_error)?;
        if tombstone.is_some_and(|purge_at| purge_at >= 0 && now < purge_at as u64) {
            return Err(Failure::new(
                FailureCode::ArtifactExpired,
                "artifact retention expired",
            ));
        }
        Err(Failure::new(
            FailureCode::ArtifactUnknown,
            "artifact does not exist",
        ))
    }

    fn open(&self, check_integrity: bool) -> Result<Connection, Failure> {
        let parent = self.path.parent().ok_or_else(|| {
            Failure::new(
                FailureCode::UnsafeRoot,
                "artifact store path has no parent directory",
            )
        })?;
        secure_store_root(parent)?;
        validate_optional_store_file(&self.path)?;
        for suffix in ["-wal", "-shm"] {
            validate_optional_store_file(&PathBuf::from(format!(
                "{}{suffix}",
                self.path.display()
            )))?;
        }
        let mut connection = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_FULL_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(map_open_error)?;
        connection
            .busy_timeout(Duration::from_millis(self.busy_timeout_ms))
            .map_err(map_open_error)?;
        let schema_version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(map_open_error)?;
        if schema_version > STORE_SCHEMA_VERSION {
            return Err(Failure::new(
                FailureCode::ArtifactSchemaUnsupported,
                "artifact database schema is newer than this engine",
            ));
        }
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(map_open_error)?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(map_open_error)?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(map_open_error)?;
        if schema_version == 0 {
            connection
                .execute_batch(
                    "BEGIN IMMEDIATE;
                    CREATE TABLE artifacts (
                    id TEXT PRIMARY KEY,
                    schema_version TEXT NOT NULL,
                    state TEXT NOT NULL,
                    source BLOB NOT NULL,
                    sha256 TEXT NOT NULL,
                    source_bytes INTEGER NOT NULL,
                    source_metadata BLOB NOT NULL,
                    source_metadata_sha256 TEXT NOT NULL,
                    lineage_count INTEGER NOT NULL,
                    lineage_bytes INTEGER NOT NULL,
                    lineage_head_sha256 TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    expires_at INTEGER NOT NULL
                );
                CREATE INDEX artifacts_expiry
                    ON artifacts(expires_at);
                CREATE TABLE tombstones (
                    id TEXT PRIMARY KEY,
                    schema_version TEXT NOT NULL,
                    expired_at INTEGER NOT NULL,
                    purge_at INTEGER NOT NULL,
                    failure_code TEXT NOT NULL
                );
                CREATE TABLE artifact_receipts (
                    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                    artifact_id TEXT NOT NULL,
                    lineage_sequence INTEGER NOT NULL,
                    request_id TEXT NOT NULL,
                    receipt_metadata BLOB NOT NULL,
                    receipt_metadata_sha256 TEXT NOT NULL,
                    lineage_chain_sha256 TEXT NOT NULL,
                    UNIQUE (artifact_id, lineage_sequence),
                    FOREIGN KEY (artifact_id) REFERENCES artifacts(id) ON DELETE CASCADE
                );
                CREATE INDEX artifact_receipts_lineage
                    ON artifact_receipts(artifact_id, lineage_sequence);
                PRAGMA user_version = 3;
                COMMIT;",
                )
                .map_err(map_open_error)?;
        } else if schema_version == 1 {
            connection
                .execute_batch(
                    "BEGIN IMMEDIATE;
                    CREATE TABLE artifact_receipts (
                        sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                        artifact_id TEXT NOT NULL,
                        request_id TEXT NOT NULL,
                        receipt_metadata BLOB NOT NULL,
                        FOREIGN KEY (artifact_id) REFERENCES artifacts(id) ON DELETE CASCADE
                    );
                    CREATE INDEX artifact_receipts_lineage
                        ON artifact_receipts(artifact_id, sequence);
                    PRAGMA user_version = 2;
                    COMMIT;",
                )
                .map_err(map_open_error)?;
            migrate_v2_to_v3(&mut connection, self.max_lineage_bytes)?;
        } else if schema_version == 2 {
            migrate_v2_to_v3(&mut connection, self.max_lineage_bytes)?;
        }
        if check_integrity {
            let integrity: String = connection
                .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
                .map_err(map_open_error)?;
            if integrity != "ok" {
                return Err(Failure::new(
                    FailureCode::ArtifactCorrupt,
                    "artifact database integrity check failed",
                ));
            }
            validate_lineage_bounds_connection(&connection, self.max_lineage_bytes)?;
        }
        enforce_store_modes(&self.path)?;
        Ok(connection)
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
    let record = connection
        .query_row(
            "SELECT schema_version, state, source, sha256, source_bytes,
                    source_metadata, source_metadata_sha256, created_at, expires_at
             FROM artifacts WHERE id = ?1",
            [&reference.id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            },
        )
        .optional()
        .map_err(map_read_error)?;
    let Some((
        schema,
        state,
        bytes,
        digest,
        source_bytes,
        acquisition_json,
        acquisition_digest,
        created_at,
        expires_at,
    )) = record
    else {
        return ArtifactStore::missing_artifact(connection, &reference.id, now);
    };
    if schema != ARTIFACT_SCHEMA_VERSION {
        return Err(Failure::new(
            FailureCode::ArtifactSchemaUnsupported,
            "stored artifact schema is unsupported",
        ));
    }
    if state != "committed" {
        return Err(Failure::new(
            FailureCode::CommitFailed,
            "artifact transaction did not reach committed state",
        ));
    }
    let source_bytes = nonnegative_u64(source_bytes, "source byte count")?;
    let created_at = nonnegative_u64(created_at, "creation time")?;
    let expires_at = nonnegative_u64(expires_at, "expiration time")?;
    let stored_reference = ArtifactRef {
        schema_version: schema,
        id: reference.id.clone(),
        source_sha256: digest.clone(),
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
    if reference.source_sha256 != digest
        || reference.source_bytes != source_bytes
        || reference.created_at != created_at
        || reference.expires_at != expires_at
        || bytes.len() as u64 != source_bytes
        || sha256_hex(&bytes) != digest
    {
        return Err(Failure::new(
            FailureCode::ArtifactCorrupt,
            "artifact integrity verification failed",
        )
        .with_artifact(stored_reference));
    }
    verify_exact_digest(
        &acquisition_json,
        &acquisition_digest,
        "artifact acquisition metadata integrity verification failed",
        &stored_reference,
    )?;
    let acquisition: AcquisitionReceipt =
        serde_json::from_slice(&acquisition_json).map_err(|_| {
            Failure::new(
                FailureCode::ArtifactCorrupt,
                "artifact acquisition metadata is corrupt",
            )
            .with_artifact(stored_reference.clone())
        })?;
    acquisition.validate(source_bytes).map_err(|_| {
        Failure::new(
            FailureCode::ArtifactCorrupt,
            "artifact acquisition metadata is semantically corrupt",
        )
        .with_artifact(stored_reference)
    })?;
    Ok(StoredArtifact { bytes, acquisition })
}

#[derive(Debug)]
struct LineageShape {
    count: u64,
    bytes: u64,
    last_chain: Option<String>,
}

fn lineage_shape(
    connection: &Connection,
    reference: &ArtifactRef,
) -> Result<LineageShape, Failure> {
    let (count, bytes, minimum, maximum, distinct): (i64, i64, i64, i64, i64) = connection
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(length(receipt_metadata)), 0),
                    COALESCE(MIN(lineage_sequence), -1),
                    COALESCE(MAX(lineage_sequence), -1),
                    COUNT(DISTINCT lineage_sequence)
             FROM artifact_receipts WHERE artifact_id = ?1",
            [&reference.id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .map_err(map_read_error)?;
    let count = nonnegative_u64(count, "receipt row count")?;
    let bytes = nonnegative_u64(bytes, "artifact lineage usage")?;
    let distinct = nonnegative_u64(distinct, "distinct receipt sequence count")?;
    let expected_max = i64::try_from(count)
        .ok()
        .and_then(|count| count.checked_sub(1))
        .unwrap_or(i64::MAX);
    if distinct != count
        || (count == 0 && (minimum != -1 || maximum != -1))
        || (count > 0 && (minimum != 0 || maximum != expected_max))
    {
        return Err(Failure::new(
            FailureCode::ArtifactCorrupt,
            "artifact receipt lineage sequence is corrupt",
        )
        .with_artifact(reference.clone()));
    }
    let last_chain = if count == 0 {
        None
    } else {
        connection
            .query_row(
                "SELECT lineage_chain_sha256 FROM artifact_receipts
                 WHERE artifact_id = ?1 ORDER BY lineage_sequence DESC LIMIT 1",
                [&reference.id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(map_read_error)?
    };
    if last_chain
        .as_deref()
        .is_some_and(|digest| !valid_sha256(digest))
    {
        return Err(Failure::new(
            FailureCode::ArtifactCorrupt,
            "artifact receipt lineage commitment is malformed",
        )
        .with_artifact(reference.clone()));
    }
    Ok(LineageShape {
        count,
        bytes,
        last_chain,
    })
}

fn validate_lineage_bounds_connection(
    connection: &Connection,
    max_lineage_bytes: u64,
) -> Result<(), Failure> {
    if lineage_usage_connection(connection)? > max_lineage_bytes {
        return Err(Failure::new(
            FailureCode::ArtifactCorrupt,
            "artifact store lineage exceeds its global logical cap",
        ));
    }
    let oversized = connection
        .query_row(
            "SELECT artifact_id FROM artifact_receipts
             GROUP BY artifact_id
             HAVING SUM(length(receipt_metadata)) > ?1
             LIMIT 1",
            [i64::try_from(MAX_ARTIFACT_LINEAGE_BYTES).map_err(|_| {
                Failure::new(
                    FailureCode::InvariantBreach,
                    "per-artifact lineage cap is not representable",
                )
            })?],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(map_read_error)?;
    if oversized.is_some() {
        return Err(Failure::new(
            FailureCode::ArtifactCorrupt,
            "artifact receipt lineage exceeds its per-artifact logical cap",
        ));
    }
    Ok(())
}

fn store_usage(transaction: &Transaction<'_>) -> Result<u64, Failure> {
    let used: i64 = transaction
        .query_row(
            "SELECT COALESCE(SUM(source_bytes), 0) FROM artifacts",
            [],
            |row| row.get(0),
        )
        .map_err(map_write_error)?;
    nonnegative_u64(used, "store usage")
}

fn lineage_usage(transaction: &Transaction<'_>) -> Result<u64, Failure> {
    let used: i64 = transaction
        .query_row(
            "SELECT COALESCE(SUM(length(receipt_metadata)), 0)
             FROM artifact_receipts",
            [],
            |row| row.get(0),
        )
        .map_err(map_write_error)?;
    nonnegative_u64(used, "lineage usage")
}

fn lineage_usage_connection(connection: &Connection) -> Result<u64, Failure> {
    let used: i64 = connection
        .query_row(
            "SELECT COALESCE(SUM(length(receipt_metadata)), 0)
             FROM artifact_receipts",
            [],
            |row| row.get(0),
        )
        .map_err(map_read_error)?;
    nonnegative_u64(used, "lineage usage")
}

fn collect_in_transaction(transaction: &Transaction<'_>, now: u64) -> Result<GcReport, Failure> {
    let now_sql = i64::try_from(now).map_err(|_| {
        Failure::new(
            FailureCode::InvalidRequest,
            "garbage collection time exceeds SQLite limits",
        )
    })?;
    let (records, bytes): (i64, i64) = transaction
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(source_bytes), 0)
             FROM artifacts WHERE expires_at <= ?1",
            [now_sql],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(map_write_error)?;
    let lineage_bytes: i64 = transaction
        .query_row(
            "SELECT COALESCE(SUM(length(receipt_metadata)), 0)
             FROM artifact_receipts
             WHERE artifact_id IN (
                 SELECT id FROM artifacts WHERE expires_at <= ?1
             )",
            [now_sql],
            |row| row.get(0),
        )
        .map_err(map_write_error)?;
    let purge_at = now.saturating_add(TOMBSTONE_TTL_SECONDS);
    let purge_at_sql = i64::try_from(purge_at).map_err(|_| {
        Failure::new(
            FailureCode::InvalidRequest,
            "tombstone expiration exceeds SQLite limits",
        )
    })?;
    transaction
        .execute(
            "INSERT INTO tombstones
                (id, schema_version, expired_at, purge_at, failure_code)
             SELECT id, 'distill.tombstone/v1', expires_at, ?2, 'artifact_expired'
             FROM artifacts WHERE expires_at <= ?1
             ON CONFLICT(id) DO UPDATE SET
                schema_version = excluded.schema_version,
                expired_at = excluded.expired_at,
                purge_at = excluded.purge_at,
                failure_code = excluded.failure_code",
            params![now_sql, purge_at_sql],
        )
        .map_err(map_write_error)?;
    transaction
        .execute("DELETE FROM artifacts WHERE expires_at <= ?1", [now_sql])
        .map_err(map_write_error)?;
    transaction
        .execute("DELETE FROM tombstones WHERE purge_at <= ?1", [now_sql])
        .map_err(map_write_error)?;
    Ok(GcReport {
        reclaimed_bytes: nonnegative_u64(bytes, "garbage collection bytes")?,
        reclaimed_lineage_bytes: nonnegative_u64(
            lineage_bytes,
            "garbage collection lineage bytes",
        )?,
        reclaimed_records: nonnegative_u64(records, "garbage collection records")?,
    })
}

fn verify_exact_digest(
    bytes: &[u8],
    digest: &str,
    message: &'static str,
    reference: &ArtifactRef,
) -> Result<(), Failure> {
    if !valid_sha256(digest) || sha256_hex(bytes) != digest {
        return Err(
            Failure::new(FailureCode::ArtifactCorrupt, message).with_artifact(reference.clone())
        );
    }
    Ok(())
}

fn valid_sha256(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn lineage_chain_sha256(previous: &str, sequence: u64, receipt_json: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(LINEAGE_DIGEST_DOMAIN);
    digest.update(previous.as_bytes());
    digest.update(sequence.to_be_bytes());
    digest.update(receipt_json);
    format!("{:x}", digest.finalize())
}

fn validate_receipt(
    receipt: &Receipt,
    reference: &ArtifactRef,
    code: FailureCode,
) -> Result<(), Failure> {
    if receipt.schema_version != RECEIPT_SCHEMA_VERSION
        || receipt.artifact != *reference
        || receipt.source_sha256 != reference.source_sha256
        || receipt.projection_version != PROJECTION_VERSION
        || receipt.policy_version != POLICY_VERSION
        || receipt.request_id.is_empty()
        || receipt.request_id.len() > crate::request_policy::MAX_IDENTIFIER_BYTES
        || receipt.preservation.profile.is_empty()
        || receipt.preservation.profile.len() > crate::request_policy::MAX_IDENTIFIER_BYTES
        || receipt.preservation.mandatory_fact_ids.len() > 256
        || receipt
            .preservation
            .mandatory_fact_ids
            .iter()
            .any(|id| id.is_empty() || id.len() > crate::request_policy::MAX_IDENTIFIER_BYTES)
        || receipt.retained_spans.len() > MAX_RECEIPT_SPANS
        || receipt.omitted_spans.len() > MAX_RECEIPT_SPANS
        || matches!(receipt.count_unit, CountUnit::Bytes) && receipt.token_profile.is_some()
        || matches!(receipt.count_unit, CountUnit::Tokens)
            && receipt.token_profile.as_deref() != Some(CL100K_PROFILE)
        || !receipt_spans_are_consistent(receipt, reference.source_bytes)
    {
        return Err(
            Failure::new(code, "artifact projection receipt lineage is inconsistent")
                .with_artifact(reference.clone()),
        );
    }
    receipt
        .acquisition
        .validate(reference.source_bytes)
        .map_err(|_| {
            Failure::new(
                code,
                "artifact projection receipt acquisition is contradictory",
            )
            .with_artifact(reference.clone())
        })
}

fn receipt_spans_are_consistent(receipt: &Receipt, source_bytes: u64) -> bool {
    let sorted = |spans: &[crate::types::ByteSpan]| {
        let mut previous_end = 0_u64;
        for span in spans {
            if span.start < previous_end
                || span.end < span.start
                || span.end > source_bytes
                || (span.start == span.end && source_bytes != 0)
            {
                return false;
            }
            previous_end = span.end;
        }
        true
    };
    if !sorted(&receipt.retained_spans) || !sorted(&receipt.omitted_spans) {
        return false;
    }
    let mut partition = receipt
        .retained_spans
        .iter()
        .chain(&receipt.omitted_spans)
        .copied()
        .collect::<Vec<_>>();
    partition.sort_by_key(|span| (span.start, span.end));
    let mut cursor = 0_u64;
    for span in partition {
        if span.start != cursor {
            return false;
        }
        cursor = span.end;
    }
    let covers_source = cursor == source_bytes;
    match receipt.fidelity {
        Fidelity::Exact => receipt.omitted_spans.is_empty() && covers_source,
        Fidelity::Extractive => covers_source,
        Fidelity::Encoded | Fidelity::MetadataOnly => {
            receipt.retained_spans.is_empty() && covers_source
        }
    }
}

#[derive(Debug)]
struct MigratedLineage {
    count: u64,
    bytes: u64,
    head: String,
}

fn migrate_v2_to_v3(connection: &mut Connection, max_lineage_bytes: u64) -> Result<(), Failure> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_write_error)?;
    transaction
        .execute_batch(
            "ALTER TABLE artifacts ADD COLUMN source_metadata_sha256 TEXT;
             ALTER TABLE artifacts ADD COLUMN lineage_count INTEGER;
             ALTER TABLE artifacts ADD COLUMN lineage_bytes INTEGER;
             ALTER TABLE artifacts ADD COLUMN lineage_head_sha256 TEXT;
             ALTER TABLE artifact_receipts ADD COLUMN lineage_sequence INTEGER;
             ALTER TABLE artifact_receipts ADD COLUMN receipt_metadata_sha256 TEXT;
             ALTER TABLE artifact_receipts ADD COLUMN lineage_chain_sha256 TEXT;",
        )
        .map_err(map_write_error)?;
    let migrated_lineage = lineage_usage(&transaction)?;
    if migrated_lineage > max_lineage_bytes {
        return Err(Failure::new(
            FailureCode::ArtifactCorrupt,
            "v2 receipt lineage exceeds the configured global cap",
        ));
    }
    let oversized_artifact = transaction
        .query_row(
            "SELECT artifact_id FROM artifact_receipts
             GROUP BY artifact_id
             HAVING SUM(length(receipt_metadata)) > ?1
             LIMIT 1",
            [i64::try_from(MAX_ARTIFACT_LINEAGE_BYTES).map_err(|_| {
                Failure::new(
                    FailureCode::InvariantBreach,
                    "per-artifact lineage cap is not representable",
                )
            })?],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(map_read_error)?;
    if oversized_artifact.is_some() {
        return Err(Failure::new(
            FailureCode::ArtifactCorrupt,
            "v2 receipt lineage exceeds the per-artifact cap",
        ));
    }

    let artifacts = {
        let mut statement = transaction
            .prepare(
                "SELECT id, schema_version, state, source, sha256, source_bytes,
                        source_metadata, created_at, expires_at
                 FROM artifacts",
            )
            .map_err(map_read_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Vec<u8>>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            })
            .map_err(map_read_error)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(map_read_error)?
    };
    let mut references = std::collections::BTreeMap::new();
    for (
        id,
        schema,
        state,
        source,
        source_digest,
        source_bytes,
        acquisition_json,
        created_at,
        expires_at,
    ) in artifacts
    {
        let source_bytes = nonnegative_u64(source_bytes, "source byte count")?;
        let created_at = nonnegative_u64(created_at, "creation time")?;
        let expires_at = nonnegative_u64(expires_at, "expiration time")?;
        let reference = ArtifactRef {
            schema_version: schema,
            id: id.clone(),
            source_sha256: source_digest.clone(),
            source_bytes,
            created_at,
            expires_at,
        };
        if reference.schema_version != ARTIFACT_SCHEMA_VERSION
            || state != "committed"
            || source.len() as u64 != source_bytes
            || sha256_hex(&source) != source_digest
        {
            return Err(
                Failure::new(FailureCode::ArtifactCorrupt, "v2 artifact state is corrupt")
                    .with_artifact(reference),
            );
        }
        let acquisition: AcquisitionReceipt =
            serde_json::from_slice(&acquisition_json).map_err(|_| {
                Failure::new(
                    FailureCode::ArtifactCorrupt,
                    "v2 acquisition metadata is corrupt",
                )
                .with_artifact(reference.clone())
            })?;
        acquisition.validate(source_bytes).map_err(|_| {
            Failure::new(
                FailureCode::ArtifactCorrupt,
                "v2 acquisition metadata is semantically corrupt",
            )
            .with_artifact(reference.clone())
        })?;
        transaction
            .execute(
                "UPDATE artifacts SET source_metadata_sha256 = ?1 WHERE id = ?2",
                params![sha256_hex(&acquisition_json), id],
            )
            .map_err(map_write_error)?;
        references.insert(id, reference);
    }

    let receipt_rows = {
        let mut statement = transaction
            .prepare(
                "SELECT sequence, artifact_id, request_id, receipt_metadata
                 FROM artifact_receipts ORDER BY artifact_id, sequence",
            )
            .map_err(map_read_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            })
            .map_err(map_read_error)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(map_read_error)?
    };
    let mut lineage = references
        .keys()
        .map(|id| {
            (
                id.clone(),
                MigratedLineage {
                    count: 0,
                    bytes: 0,
                    head: EMPTY_LINEAGE_SHA256.to_owned(),
                },
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    for (row_sequence, artifact_id, request_id, receipt_json) in receipt_rows {
        let reference = references.get(&artifact_id).ok_or_else(|| {
            Failure::new(
                FailureCode::ArtifactCorrupt,
                "v2 receipt names a missing artifact",
            )
        })?;
        let receipt: Receipt = serde_json::from_slice(&receipt_json).map_err(|_| {
            Failure::new(
                FailureCode::ArtifactCorrupt,
                "v2 projection receipt is corrupt",
            )
            .with_artifact(reference.clone())
        })?;
        if receipt.request_id != request_id {
            return Err(Failure::new(
                FailureCode::ArtifactCorrupt,
                "v2 receipt request binding is corrupt",
            )
            .with_artifact(reference.clone()));
        }
        validate_receipt(&receipt, reference, FailureCode::ArtifactCorrupt)?;
        let lineage = lineage.get_mut(&artifact_id).ok_or_else(|| {
            Failure::new(
                FailureCode::ArtifactCorrupt,
                "v2 receipt names a missing artifact",
            )
        })?;
        let sequence_sql = i64::try_from(lineage.count).map_err(|_| {
            Failure::new(
                FailureCode::ArtifactCorrupt,
                "v2 receipt sequence is not representable",
            )
        })?;
        let chain = lineage_chain_sha256(&lineage.head, lineage.count, &receipt_json);
        transaction
            .execute(
                "UPDATE artifact_receipts
                 SET lineage_sequence = ?1, receipt_metadata_sha256 = ?2,
                     lineage_chain_sha256 = ?3
                 WHERE sequence = ?4",
                params![sequence_sql, sha256_hex(&receipt_json), chain, row_sequence],
            )
            .map_err(map_write_error)?;
        lineage.count = lineage.count.checked_add(1).ok_or_else(|| {
            Failure::new(
                FailureCode::ArtifactCorrupt,
                "v2 receipt sequence is exhausted",
            )
        })?;
        lineage.bytes = lineage
            .bytes
            .checked_add(receipt_json.len() as u64)
            .ok_or_else(|| {
                Failure::new(
                    FailureCode::ArtifactCorrupt,
                    "v2 receipt lineage size is not representable",
                )
            })?;
        lineage.head = chain;
    }
    for (artifact_id, lineage) in lineage {
        transaction
            .execute(
                "UPDATE artifacts
                 SET lineage_count = ?1, lineage_bytes = ?2, lineage_head_sha256 = ?3
                 WHERE id = ?4",
                params![
                    i64::try_from(lineage.count).map_err(|_| {
                        Failure::new(
                            FailureCode::ArtifactCorrupt,
                            "v2 receipt sequence is not representable",
                        )
                    })?,
                    i64::try_from(lineage.bytes).map_err(|_| {
                        Failure::new(
                            FailureCode::ArtifactCorrupt,
                            "v2 receipt lineage size is not representable",
                        )
                    })?,
                    lineage.head,
                    artifact_id,
                ],
            )
            .map_err(map_write_error)?;
    }
    transaction
        .execute_batch(
            "CREATE UNIQUE INDEX artifact_receipts_artifact_sequence
                 ON artifact_receipts(artifact_id, lineage_sequence);
             PRAGMA user_version = 3;",
        )
        .map_err(map_write_error)?;
    transaction.commit().map_err(map_write_error)?;
    Ok(())
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

fn secure_store_root(path: &Path) -> Result<(), Failure> {
    let existed = path.exists();
    fs::create_dir_all(path).map_err(|_| {
        Failure::new(
            FailureCode::PermissionDenied,
            "artifact store root cannot be created",
        )
    })?;
    let metadata = fs::symlink_metadata(path).map_err(|_| {
        Failure::new(
            FailureCode::PermissionDenied,
            "artifact store root cannot be inspected",
        )
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Failure::new(
            FailureCode::UnsafeRoot,
            "artifact store root must be a real directory",
        ));
    }
    validate_store_owner(&metadata, "artifact store root")?;
    verify_no_store_symlinks(path)?;
    if existed {
        let mode = store_mode(&metadata);
        if mode != 0o700 {
            return Err(Failure::new(
                FailureCode::PermissionDenied,
                "existing artifact store root is not mode 0700",
            ));
        }
        Ok(())
    } else {
        set_mode(path, 0o700)
    }
}

#[cfg(unix)]
fn validate_store_owner(metadata: &fs::Metadata, label: &str) -> Result<(), Failure> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions and does not dereference memory.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(Failure::new(
            FailureCode::PermissionDenied,
            format!("{label} is not owned by the current user"),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_store_owner(_metadata: &fs::Metadata, _label: &str) -> Result<(), Failure> {
    Err(Failure::new(
        FailureCode::PermissionDenied,
        "POSIX ownership enforcement is unavailable",
    ))
}

fn verify_no_store_symlinks(path: &Path) -> Result<(), Failure> {
    let mut current = PathBuf::from("/");
    for component in path.components() {
        match component {
            std::path::Component::RootDir => continue,
            std::path::Component::Normal(value) => current.push(value),
            _ => {
                return Err(Failure::new(
                    FailureCode::UnsafeRoot,
                    "artifact store root contains an unsafe component",
                ));
            }
        }
        if fs::symlink_metadata(&current)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(Failure::new(
                FailureCode::UnsafeRoot,
                "artifact store root traverses a symlink",
            ));
        }
    }
    Ok(())
}

fn validate_store_file(path: &Path) -> Result<(), Failure> {
    let metadata = fs::symlink_metadata(path).map_err(|_| {
        Failure::new(
            FailureCode::PermissionDenied,
            "artifact store file cannot be inspected",
        )
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(Failure::new(
            FailureCode::UnsafeRoot,
            "artifact store file must be a regular file",
        ));
    }
    validate_store_owner(&metadata, "artifact store file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(Failure::new(
                FailureCode::UnsafeRoot,
                "artifact store file must not be hard-linked",
            ));
        }
    }
    Ok(())
}

fn validate_optional_store_file(path: &Path) -> Result<(), Failure> {
    match fs::symlink_metadata(path) {
        Ok(_) => validate_store_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(Failure::new(
            FailureCode::PermissionDenied,
            "artifact store file cannot be inspected",
        )),
    }
}

#[cfg(unix)]
fn store_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn store_mode(_metadata: &fs::Metadata) -> u32 {
    0
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), Failure> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|_| {
        Failure::new(
            FailureCode::PermissionDenied,
            "artifact store permissions cannot be enforced",
        )
    })?;
    let actual = fs::metadata(path)
        .map_err(|_| {
            Failure::new(
                FailureCode::PermissionDenied,
                "artifact store permissions cannot be verified",
            )
        })?
        .permissions()
        .mode()
        & 0o777;
    if actual != mode {
        return Err(Failure::new(
            FailureCode::PermissionDenied,
            "artifact store permissions are not private",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), Failure> {
    Err(Failure::new(
        FailureCode::PermissionDenied,
        "POSIX permission enforcement is unavailable",
    ))
}

fn enforce_store_modes(path: &Path) -> Result<(), Failure> {
    validate_store_file(path)?;
    set_mode(path, 0o600)?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", path.display()));
        if fs::symlink_metadata(&sidecar).is_ok() {
            validate_store_file(&sidecar)?;
            set_mode(&sidecar, 0o600)?;
        }
    }
    Ok(())
}

fn map_open_error(error: rusqlite::Error) -> Failure {
    match sqlite_code(&error) {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => {
            Failure::new(FailureCode::StoreBusy, "artifact store is busy")
        }
        Some(ErrorCode::PermissionDenied | ErrorCode::ReadOnly) => Failure::new(
            FailureCode::PermissionDenied,
            "artifact store is not writable",
        ),
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) => {
            Failure::new(FailureCode::ArtifactCorrupt, "artifact database is corrupt")
        }
        Some(ErrorCode::DiskFull) => Failure::new(FailureCode::StoreFull, "artifact store is full"),
        _ => Failure::new(
            FailureCode::CommitFailed,
            "artifact store initialization failed",
        ),
    }
}

fn map_write_error(error: rusqlite::Error) -> Failure {
    match sqlite_code(&error) {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => {
            Failure::new(FailureCode::StoreBusy, "artifact transaction timed out")
        }
        Some(ErrorCode::PermissionDenied | ErrorCode::ReadOnly) => Failure::new(
            FailureCode::PermissionDenied,
            "artifact transaction is not writable",
        ),
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) => {
            Failure::new(FailureCode::ArtifactCorrupt, "artifact database is corrupt")
        }
        Some(ErrorCode::DiskFull) => Failure::new(FailureCode::StoreFull, "artifact store is full"),
        _ => Failure::new(FailureCode::CommitFailed, "artifact transaction failed"),
    }
}

fn map_read_error(error: rusqlite::Error) -> Failure {
    match sqlite_code(&error) {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => {
            Failure::new(FailureCode::StoreBusy, "artifact store is busy")
        }
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) => {
            Failure::new(FailureCode::ArtifactCorrupt, "artifact database is corrupt")
        }
        _ => Failure::new(
            FailureCode::ArtifactCorrupt,
            "artifact record cannot be decoded",
        ),
    }
}

fn sqlite_code(error: &rusqlite::Error) -> Option<ErrorCode> {
    match error {
        rusqlite::Error::SqliteFailure(code, _) => Some(code.code),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        AcquisitionReceipt, ByteSpan, CountUnit, Fidelity, MAX_LINEAGE_BYTES, POLICY_VERSION,
        PROJECTION_VERSION, PreservationResult, SourceVariant,
    };
    use std::{
        os::unix::fs::{PermissionsExt, symlink},
        sync::{Arc, Barrier},
        thread,
    };
    use tempfile::TempDir;

    fn receipt() -> AcquisitionReceipt {
        AcquisitionReceipt {
            variant: SourceVariant::Inline,
            complete: true,
            partial: false,
            truncated: false,
            root_id: None,
            relative_path: None,
            process: None,
        }
    }

    fn fixture(max_bytes: u64) -> (TempDir, ArtifactStore) {
        let directory = tempfile::tempdir().expect("temp directory");
        let store = ArtifactStore::new(
            directory.path().join("private/store.sqlite"),
            max_bytes,
            crate::types::MAX_LINEAGE_BYTES,
            250,
        );
        store.initialize().expect("initialize store");
        (directory, store)
    }

    fn commit(store: &ArtifactStore, id: u64, bytes: &[u8], now: u64) -> ArtifactRef {
        store
            .commit(
                &format!("{id:032x}"),
                bytes,
                &receipt(),
                now,
                now + 100,
                None,
            )
            .expect("commit")
    }

    #[test]
    fn commit_rejects_each_invalid_boundary_before_persistence() {
        let (_directory, store) = fixture(0);

        assert_eq!(
            store
                .commit("short", b"", &receipt(), 1, 2, None)
                .expect_err("artifact ID length")
                .code,
            FailureCode::InvariantBreach
        );
        assert_eq!(
            store
                .commit(&"G".repeat(32), b"", &receipt(), 1, 2, None)
                .expect_err("artifact ID alphabet")
                .code,
            FailureCode::InvariantBreach
        );
        assert_eq!(
            store
                .commit(&"1".repeat(32), b"", &receipt(), u64::MAX, 2, None)
                .expect_err("creation timestamp")
                .code,
            FailureCode::InvalidRequest
        );
        assert_eq!(
            store
                .commit(&"1".repeat(32), b"", &receipt(), 1, u64::MAX, None)
                .expect_err("expiration timestamp")
                .code,
            FailureCode::InvalidRequest
        );
        assert_eq!(
            store
                .commit(&"1".repeat(32), b"", &receipt(), 2, 2, None)
                .expect_err("expiration order")
                .code,
            FailureCode::InvalidRequest
        );

        let mut contradictory = receipt();
        contradictory.partial = true;
        assert_eq!(
            store
                .commit(&"1".repeat(32), b"", &contradictory, 1, 2, None)
                .expect_err("acquisition semantics")
                .code,
            FailureCode::InvariantBreach
        );
        assert_eq!(
            store
                .commit(&"1".repeat(32), b"x", &receipt(), 1, 2, None)
                .expect_err("store capacity")
                .code,
            FailureCode::StoreFull
        );
    }

    fn projection_receipt(reference: &ArtifactRef, request_id: String) -> Receipt {
        Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION.to_owned(),
            request_id,
            source_sha256: reference.source_sha256.clone(),
            artifact: reference.clone(),
            projection_version: PROJECTION_VERSION.to_owned(),
            policy_version: POLICY_VERSION.to_owned(),
            token_profile: None,
            original_count: reference.source_bytes,
            visible_count: reference.source_bytes,
            count_unit: CountUnit::Bytes,
            fidelity: Fidelity::Exact,
            retained_spans: vec![ByteSpan {
                start: 0,
                end: reference.source_bytes,
            }],
            omitted_spans: Vec::new(),
            preservation: PreservationResult {
                profile: "plain-text/v1".to_owned(),
                mandatory_fact_ids: Vec::new(),
            },
            acquisition: receipt(),
        }
    }

    fn legacy_v2_fixture(
        acquisition: &AcquisitionReceipt,
        receipt_count: usize,
    ) -> (TempDir, PathBuf, ArtifactRef, Vec<u8>, Vec<Vec<u8>>) {
        let directory = tempfile::tempdir().expect("temp directory");
        let private = directory.path().join("private");
        fs::create_dir(&private).expect("private directory");
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700))
            .expect("private permissions");
        let path = private.join("store.sqlite");
        let connection = Connection::open(&path).expect("legacy database");
        connection
            .execute_batch(
                "CREATE TABLE artifacts (
                    id TEXT PRIMARY KEY,
                    schema_version TEXT NOT NULL,
                    state TEXT NOT NULL,
                    source BLOB NOT NULL,
                    sha256 TEXT NOT NULL,
                    source_bytes INTEGER NOT NULL,
                    source_metadata BLOB NOT NULL,
                    created_at INTEGER NOT NULL,
                    expires_at INTEGER NOT NULL
                );
                CREATE INDEX artifacts_expiry ON artifacts(expires_at);
                CREATE TABLE tombstones (
                    id TEXT PRIMARY KEY,
                    schema_version TEXT NOT NULL,
                    expired_at INTEGER NOT NULL,
                    purge_at INTEGER NOT NULL,
                    failure_code TEXT NOT NULL
                );
                CREATE TABLE artifact_receipts (
                    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                    artifact_id TEXT NOT NULL,
                    request_id TEXT NOT NULL,
                    receipt_metadata BLOB NOT NULL,
                    FOREIGN KEY (artifact_id) REFERENCES artifacts(id) ON DELETE CASCADE
                );
                CREATE INDEX artifact_receipts_lineage
                    ON artifact_receipts(artifact_id, sequence);
                PRAGMA user_version = 2;",
            )
            .expect("legacy schema");
        let bytes = b"legacy".to_vec();
        let reference = ArtifactRef {
            schema_version: ARTIFACT_SCHEMA_VERSION.to_owned(),
            id: "1".repeat(32),
            source_sha256: sha256_hex(&bytes),
            source_bytes: bytes.len() as u64,
            created_at: 100,
            expires_at: 200,
        };
        let acquisition_json = serde_json::to_vec(acquisition).expect("legacy acquisition");
        connection
            .execute(
                "INSERT INTO artifacts
                 (id, schema_version, state, source, sha256, source_bytes,
                  source_metadata, created_at, expires_at)
                 VALUES (?1, ?2, 'committed', ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    reference.id,
                    reference.schema_version,
                    bytes,
                    reference.source_sha256,
                    i64::try_from(reference.source_bytes).expect("legacy source bytes"),
                    acquisition_json,
                    i64::try_from(reference.created_at).expect("legacy creation time"),
                    i64::try_from(reference.expires_at).expect("legacy expiration time"),
                ],
            )
            .expect("legacy artifact");
        let mut receipt_blobs = Vec::new();
        for index in 0..receipt_count {
            let receipt = projection_receipt(&reference, format!("legacy-{index}"));
            let receipt_json = serde_json::to_vec(&receipt).expect("legacy receipt");
            connection
                .execute(
                    "INSERT INTO artifact_receipts
                     (artifact_id, request_id, receipt_metadata)
                     VALUES (?1, ?2, ?3)",
                    params![reference.id, receipt.request_id, receipt_json],
                )
                .expect("legacy receipt insert");
            receipt_blobs.push(receipt_json);
        }
        drop(connection);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .expect("legacy database permissions");
        (directory, path, reference, acquisition_json, receipt_blobs)
    }

    #[test]
    fn restart_recovery_is_byte_exact_and_private() {
        let (_directory, store) = fixture(1_024);
        let artifact = commit(&store, 1, b"\0source\xff", 100);
        let restarted = ArtifactStore::new(
            store.path.clone(),
            1_024,
            crate::types::MAX_LINEAGE_BYTES,
            250,
        );
        let recovered = restarted.retrieve(&artifact, 101).expect("recover");
        assert_eq!(recovered.bytes, b"\0source\xff");
        assert_eq!(recovered.acquisition, receipt());

        let directory_mode = fs::metadata(store.path.parent().expect("parent"))
            .expect("directory metadata")
            .permissions()
            .mode()
            & 0o777;
        let file_mode = fs::metadata(&store.path)
            .expect("file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }

    #[test]
    fn database_and_sidecar_symlinks_are_rejected_before_open() {
        let directory = tempfile::tempdir().expect("temp directory");
        let private = directory.path().join("private");
        fs::create_dir(&private).expect("private directory");
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700))
            .expect("private permissions");
        let database = private.join("store.sqlite");
        symlink(private.join("missing"), &database).expect("database symlink");
        let store = ArtifactStore::new(
            database.clone(),
            1_024,
            crate::types::MAX_LINEAGE_BYTES,
            250,
        );
        assert_eq!(
            store.initialize().expect_err("database symlink").code,
            FailureCode::UnsafeRoot
        );

        fs::remove_file(&database).expect("remove database symlink");
        store.initialize().expect("initialize real database");
        let sidecar = PathBuf::from(format!("{}-wal", database.display()));
        if sidecar.exists() {
            fs::remove_file(&sidecar).expect("remove real sidecar");
        }
        symlink(private.join("missing-sidecar"), &sidecar).expect("sidecar symlink");
        assert_eq!(
            store.initialize().expect_err("sidecar symlink").code,
            FailureCode::UnsafeRoot
        );
    }

    #[test]
    fn garbage_collection_only_removes_expired_sources_and_keeps_tombstones() {
        let (_directory, store) = fixture(1_024);
        let expired = store
            .commit(&"1".repeat(32), b"old", &receipt(), 10, 20, None)
            .expect("expired commit");
        let live = store
            .commit(&"2".repeat(32), b"live", &receipt(), 10, 200, None)
            .expect("live commit");
        let report = store.collect_garbage(20).expect("collect");
        assert_eq!(
            report,
            GcReport {
                reclaimed_bytes: 3,
                reclaimed_lineage_bytes: 0,
                reclaimed_records: 1
            }
        );
        assert_eq!(
            store.retrieve(&expired, 20).expect_err("expired").code,
            FailureCode::ArtifactExpired
        );
        assert_eq!(store.retrieve(&live, 20).expect("live").bytes, b"live");
        let connection = Connection::open(&store.path).expect("connection");
        let columns = {
            let mut statement = connection
                .prepare("PRAGMA table_info(tombstones)")
                .expect("tombstone columns");
            let rows = statement
                .query_map([], |row| row.get::<_, String>(1))
                .expect("query columns");
            rows.collect::<Result<Vec<_>, _>>().expect("column names")
        };
        assert!(!columns.iter().any(|column| column.contains("sha")));
        assert!(!columns.iter().any(|column| column == "source_bytes"));
        drop(connection);

        let after_tombstone = 20 + TOMBSTONE_TTL_SECONDS + 1;
        store
            .collect_garbage(after_tombstone)
            .expect("purge tombstone");
        assert_eq!(
            store
                .retrieve(&expired, after_tombstone)
                .expect_err("unknown")
                .code,
            FailureCode::ArtifactUnknown
        );
    }

    #[test]
    fn eight_concurrent_writers_do_not_lose_commits() {
        let (_directory, store) = fixture(16 * 1_024);
        let store = Arc::new(store);
        let barrier = Arc::new(Barrier::new(8));
        let handles = (0..8)
            .map(|index| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    let bytes = format!("writer-{index}");
                    store
                        .commit(
                            &format!("{:032x}", index + 1),
                            bytes.as_bytes(),
                            &receipt(),
                            100,
                            200,
                            None,
                        )
                        .map(|artifact| (artifact, bytes))
                })
            })
            .collect::<Vec<_>>();
        let committed = handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .expect("writer thread")
                    .expect("writer commit")
            })
            .collect::<Vec<_>>();
        assert_eq!(committed.len(), 8);
        for (artifact, expected) in committed {
            assert_eq!(
                store.retrieve(&artifact, 101).expect("retrieve").bytes,
                expected.as_bytes()
            );
        }
    }

    #[test]
    fn cap_and_lock_failures_preserve_previous_artifacts() {
        let (_directory, store) = fixture(5);
        let first = commit(&store, 1, b"12345", 100);
        let full = store
            .commit(&"2".repeat(32), b"x", &receipt(), 100, 200, None)
            .expect_err("store full");
        assert_eq!(full.code, FailureCode::StoreFull);
        assert_eq!(store.retrieve(&first, 101).expect("first").bytes, b"12345");

        let locking = Connection::open(&store.path).expect("lock connection");
        locking
            .execute_batch("BEGIN IMMEDIATE")
            .expect("begin lock transaction");
        let contender = ArtifactStore::new(
            store.path.clone(),
            1_024,
            crate::types::MAX_LINEAGE_BYTES,
            1,
        );
        let busy = contender
            .commit(&"3".repeat(32), b"x", &receipt(), 100, 200, None)
            .expect_err("busy");
        assert_eq!(busy.code, FailureCode::StoreBusy);
        locking.execute_batch("ROLLBACK").expect("release lock");
        assert_eq!(
            store.retrieve(&first, 101).expect("still live").bytes,
            b"12345"
        );
    }

    #[test]
    fn transaction_faults_leave_no_partial_reference() {
        let (_directory, store) = fixture(1_024);
        for (offset, fault) in [
            CommitFault::BeforeInsert,
            CommitFault::BeforeCommit,
            CommitFault::AfterCommit,
            CommitFault::BeforeReadback,
        ]
        .into_iter()
        .enumerate()
        {
            let id = format!("{:032x}", offset + 10);
            let result = store.commit(&id, b"bytes", &receipt(), 100, 200, Some(fault));
            assert_eq!(result.expect_err("fault").code, FailureCode::CommitFailed);
            let rows: i64 = Connection::open(&store.path)
                .expect("connection")
                .query_row(
                    "SELECT COUNT(*) FROM artifacts WHERE id = ?1",
                    [&id],
                    |row| row.get(0),
                )
                .expect("count");
            if matches!(
                fault,
                CommitFault::AfterCommit | CommitFault::BeforeReadback
            ) {
                assert_eq!(rows, 1);
            } else {
                assert_eq!(rows, 0);
            }
        }
    }

    #[test]
    fn unknown_incomplete_corrupt_and_incompatible_records_are_distinct() {
        let (_directory, store) = fixture(4_096);
        let unknown = ArtifactRef {
            schema_version: ARTIFACT_SCHEMA_VERSION.to_owned(),
            id: "9".repeat(32),
            source_sha256: "0".repeat(64),
            source_bytes: 0,
            created_at: 1,
            expires_at: 2,
        };
        assert_eq!(
            store.retrieve(&unknown, 1).expect_err("unknown").code,
            FailureCode::ArtifactUnknown
        );

        let corrupt = commit(&store, 1, b"source", 100);
        let connection = Connection::open(&store.path).expect("connection");
        connection
            .execute(
                "UPDATE artifacts SET source = X'00' WHERE id = ?1",
                [&corrupt.id],
            )
            .expect("corrupt source");
        assert_eq!(
            store.retrieve(&corrupt, 101).expect_err("corrupt").code,
            FailureCode::ArtifactCorrupt
        );

        let incomplete = commit(&store, 2, b"source", 100);
        connection
            .execute(
                "UPDATE artifacts SET state = 'staging' WHERE id = ?1",
                [&incomplete.id],
            )
            .expect("mark staging");
        assert_eq!(
            store
                .retrieve(&incomplete, 101)
                .expect_err("incomplete")
                .code,
            FailureCode::CommitFailed
        );

        let incompatible = commit(&store, 3, b"source", 100);
        connection
            .execute(
                "UPDATE artifacts SET schema_version = 'distill.artifact/v2'
                 WHERE id = ?1",
                [&incompatible.id],
            )
            .expect("change schema");
        assert_eq!(
            store.retrieve(&incompatible, 101).expect_err("schema").code,
            FailureCode::ArtifactSchemaUnsupported
        );
    }

    #[test]
    fn malformed_metadata_and_database_pages_fail_closed() {
        let (directory, store) = fixture(4_096);
        let artifact = commit(&store, 1, b"source", 100);
        let connection = Connection::open(&store.path).expect("connection");
        connection
            .execute(
                "UPDATE artifacts SET source_metadata = X'FF' WHERE id = ?1",
                [&artifact.id],
            )
            .expect("corrupt metadata");
        assert_eq!(
            store.retrieve(&artifact, 101).expect_err("metadata").code,
            FailureCode::ArtifactCorrupt
        );
        drop(connection);
        drop(store);

        let corrupt_path = directory.path().join("corrupt/store.sqlite");
        fs::create_dir_all(corrupt_path.parent().expect("parent")).expect("directory");
        fs::set_permissions(
            corrupt_path.parent().expect("parent"),
            fs::Permissions::from_mode(0o700),
        )
        .expect("private directory");
        fs::write(&corrupt_path, b"not a sqlite database").expect("corrupt database");
        let corrupt_store =
            ArtifactStore::new(corrupt_path, 4_096, crate::types::MAX_LINEAGE_BYTES, 10);
        assert_eq!(
            corrupt_store.initialize().expect_err("database").code,
            FailureCode::ArtifactCorrupt
        );
    }

    #[test]
    fn v2_migration_hashes_exact_bytes_atomically_and_v3_reopen_is_read_only() {
        let (_directory, path, reference, acquisition_json, receipt_blobs) =
            legacy_v2_fixture(&receipt(), 2);
        let store = ArtifactStore::new(path.clone(), 4_096, MAX_LINEAGE_BYTES, 250);
        store.initialize().expect("migrate v2");

        let connection = Connection::open(&path).expect("inspect migrated store");
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .expect("schema version"),
            STORE_SCHEMA_VERSION
        );
        let (stored_acquisition, acquisition_digest): (Vec<u8>, String) = connection
            .query_row(
                "SELECT source_metadata, source_metadata_sha256
                 FROM artifacts WHERE id = ?1",
                [&reference.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("migrated acquisition");
        assert_eq!(stored_acquisition, acquisition_json);
        assert_eq!(acquisition_digest, sha256_hex(&acquisition_json));
        let migrated_receipts = connection
            .prepare(
                "SELECT lineage_sequence, receipt_metadata, receipt_metadata_sha256
                 FROM artifact_receipts ORDER BY lineage_sequence",
            )
            .expect("receipt query")
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .expect("receipt rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("migrated receipts");
        for (index, (sequence, bytes, digest)) in migrated_receipts.iter().enumerate() {
            assert_eq!(*sequence, index as i64);
            assert_eq!(bytes, &receipt_blobs[index]);
            assert_eq!(digest, &sha256_hex(bytes));
        }
        connection
            .execute_batch(
                "CREATE TRIGGER reject_artifact_rewrite
                 BEFORE UPDATE ON artifacts BEGIN
                     SELECT RAISE(ABORT, 'artifact rewrite');
                 END;
                 CREATE TRIGGER reject_receipt_rewrite
                 BEFORE UPDATE ON artifact_receipts BEGIN
                     SELECT RAISE(ABORT, 'receipt rewrite');
                 END;",
            )
            .expect("rewrite guards");
        drop(connection);

        store.initialize().expect("idempotent v3 reopen");
        assert_eq!(
            store.retrieve(&reference, 101).expect("retrieve").bytes,
            b"legacy"
        );
    }

    #[test]
    fn contradictory_v2_migration_rolls_back_schema_and_data() {
        let contradiction = AcquisitionReceipt {
            partial: true,
            ..receipt()
        };
        let (_directory, path, _reference, acquisition_json, _receipt_blobs) =
            legacy_v2_fixture(&contradiction, 0);
        let store = ArtifactStore::new(path.clone(), 4_096, MAX_LINEAGE_BYTES, 250);
        assert_eq!(
            store
                .initialize()
                .expect_err("contradictory migration")
                .code,
            FailureCode::ArtifactCorrupt
        );

        let connection = Connection::open(&path).expect("inspect rolled-back store");
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .expect("schema version"),
            2
        );
        let columns = connection
            .prepare("PRAGMA table_info(artifacts)")
            .expect("columns")
            .query_map([], |row| row.get::<_, String>(1))
            .expect("column rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("column names");
        assert!(!columns.iter().any(|name| name == "source_metadata_sha256"));
        assert_eq!(
            connection
                .query_row("SELECT source_metadata FROM artifacts", [], |row| {
                    row.get::<_, Vec<u8>>(0)
                })
                .expect("legacy acquisition"),
            acquisition_json
        );
    }

    #[test]
    fn oversized_or_unwritable_v2_lineage_rolls_back_migration() {
        let (_directory, path, _reference, _acquisition, receipt_blobs) =
            legacy_v2_fixture(&receipt(), 1);
        let connection = Connection::open(&path).expect("oversized legacy store");
        connection
            .execute(
                "UPDATE artifact_receipts SET receipt_metadata = ?1",
                [vec![b'x'; MAX_ARTIFACT_LINEAGE_BYTES as usize + 1]],
            )
            .expect("oversize legacy receipt");
        drop(connection);
        let store = ArtifactStore::new(path.clone(), 4_096, MAX_LINEAGE_BYTES, 250);
        assert_eq!(
            store.initialize().expect_err("oversized migration").code,
            FailureCode::ArtifactCorrupt
        );
        assert_eq!(
            Connection::open(&path)
                .expect("inspect oversized rollback")
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .expect("schema version"),
            2
        );
        assert!(!receipt_blobs.is_empty());

        let (_directory, path, _reference, _acquisition, _receipts) =
            legacy_v2_fixture(&receipt(), 1);
        let connection = Connection::open(&path).expect("guarded legacy store");
        connection
            .execute_batch(
                "CREATE TRIGGER reject_migration_write
                 BEFORE UPDATE ON artifacts BEGIN
                     SELECT RAISE(ABORT, 'migration write');
                 END;",
            )
            .expect("migration write guard");
        drop(connection);
        let store = ArtifactStore::new(path.clone(), 4_096, MAX_LINEAGE_BYTES, 250);
        assert_eq!(
            store.initialize().expect_err("write-failed migration").code,
            FailureCode::CommitFailed
        );
        let connection = Connection::open(&path).expect("inspect write rollback");
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .expect("schema version"),
            2
        );
        let columns = connection
            .prepare("PRAGMA table_info(artifacts)")
            .expect("columns")
            .query_map([], |row| row.get::<_, String>(1))
            .expect("column rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("column names");
        assert!(!columns.iter().any(|name| name == "source_metadata_sha256"));
    }

    #[test]
    fn exact_digest_and_semantic_mutations_fail_before_restore_or_trace() {
        let (_directory, store) = fixture(16 * 1_024);
        let digest_corrupt = commit(&store, 1, b"source", 100);
        let semantic_corrupt = commit(&store, 2, b"source", 100);
        let receipt_digest_corrupt = commit(&store, 3, b"source", 100);
        let receipt_semantic_corrupt = commit(&store, 4, b"source", 100);
        store
            .record_receipt(
                &receipt_digest_corrupt,
                &projection_receipt(&receipt_digest_corrupt, "receipt-digest".to_owned()),
            )
            .expect("record receipt");
        store
            .record_receipt(
                &receipt_semantic_corrupt,
                &projection_receipt(&receipt_semantic_corrupt, "receipt-semantic".to_owned()),
            )
            .expect("record receipt");

        let connection = Connection::open(&store.path).expect("mutation connection");
        let clean_failure = AcquisitionReceipt {
            complete: false,
            ..receipt()
        };
        connection
            .execute(
                "UPDATE artifacts SET source_metadata = ?1 WHERE id = ?2",
                params![
                    serde_json::to_vec(&clean_failure).expect("clean failure JSON"),
                    digest_corrupt.id
                ],
            )
            .expect("mutate acquisition bytes");
        assert_eq!(
            store
                .retrieve(&digest_corrupt, 101)
                .expect_err("acquisition digest")
                .code,
            FailureCode::ArtifactCorrupt
        );

        let contradiction = AcquisitionReceipt {
            partial: true,
            ..receipt()
        };
        let contradiction_json = serde_json::to_vec(&contradiction).expect("contradiction JSON");
        connection
            .execute(
                "UPDATE artifacts
                 SET source_metadata = ?1, source_metadata_sha256 = ?2
                 WHERE id = ?3",
                params![
                    contradiction_json,
                    sha256_hex(&contradiction_json),
                    semantic_corrupt.id
                ],
            )
            .expect("mutate acquisition semantics");
        assert_eq!(
            store
                .retrieve(&semantic_corrupt, 101)
                .expect_err("acquisition semantics")
                .code,
            FailureCode::ArtifactCorrupt
        );

        let mut changed = projection_receipt(&receipt_digest_corrupt, "changed-request".to_owned());
        changed.preservation.profile = "none/v1".to_owned();
        connection
            .execute(
                "UPDATE artifact_receipts SET receipt_metadata = ?1
                 WHERE artifact_id = ?2",
                params![
                    serde_json::to_vec(&changed).expect("changed receipt"),
                    receipt_digest_corrupt.id
                ],
            )
            .expect("mutate receipt bytes");
        assert_eq!(
            store
                .trace(&receipt_digest_corrupt, 101)
                .expect_err("receipt digest")
                .code,
            FailureCode::ArtifactCorrupt
        );

        let mut contradictory_receipt =
            projection_receipt(&receipt_semantic_corrupt, "receipt-semantic".to_owned());
        contradictory_receipt.acquisition.partial = true;
        let contradictory_json =
            serde_json::to_vec(&contradictory_receipt).expect("contradictory receipt");
        connection
            .execute(
                "UPDATE artifact_receipts
                 SET receipt_metadata = ?1, receipt_metadata_sha256 = ?2
                 WHERE artifact_id = ?3",
                params![
                    contradictory_json,
                    sha256_hex(&contradictory_json),
                    receipt_semantic_corrupt.id
                ],
            )
            .expect("mutate receipt semantics");
        assert_eq!(
            store
                .trace(&receipt_semantic_corrupt, 101)
                .expect_err("receipt semantics")
                .code,
            FailureCode::ArtifactCorrupt
        );

        assert_eq!(
            store
                .commit(&"5".repeat(32), b"source", &contradiction, 100, 200, None,)
                .expect_err("new contradiction")
                .code,
            FailureCode::InvariantBreach
        );
        let valid = commit(&store, 6, b"source", 100);
        let mut invalid_receipt = projection_receipt(&valid, "invalid".to_owned());
        invalid_receipt.acquisition.partial = true;
        assert_eq!(
            store
                .record_receipt(&valid, &invalid_receipt)
                .expect_err("new receipt contradiction")
                .code,
            FailureCode::InvariantBreach
        );
    }

    #[test]
    fn concurrent_receipts_respect_global_and_per_artifact_lineage_caps() {
        let (_directory, store) = fixture(64 * 1_024);
        let artifact = commit(&store, 1, b"source", 100);
        let mut large = projection_receipt(&artifact, "r".repeat(128));
        large.preservation.mandatory_fact_ids = (0..256)
            .map(|index| format!("{index:03}-{}", "x".repeat(124)))
            .collect();
        let receipt_bytes = serde_json::to_vec(&large).expect("large receipt").len() as u64;
        let capacity = MAX_ARTIFACT_LINEAGE_BYTES / receipt_bytes;
        assert!(capacity >= 8);
        let prefill = capacity - 4;
        for index in 0..prefill {
            let mut candidate = large.clone();
            candidate.request_id = format!("{index:0128x}");
            store
                .record_receipt(&artifact, &candidate)
                .expect("prefill lineage");
        }

        let store = Arc::new(store);
        let barrier = Arc::new(Barrier::new(8));
        let handles = (0..8)
            .map(|index| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                let artifact = artifact.clone();
                let mut candidate = large.clone();
                candidate.request_id = format!("{:0128x}", index + 1_000);
                thread::spawn(move || {
                    barrier.wait();
                    store.record_receipt(&artifact, &candidate)
                })
            })
            .collect::<Vec<_>>();
        let successes = handles
            .into_iter()
            .map(|handle| handle.join().expect("receipt writer"))
            .filter(Result::is_ok)
            .count() as u64;
        assert_eq!(successes, capacity - prefill);
        let status = store.status(101).expect("lineage status");
        assert_eq!(status.lineage_bytes, capacity * receipt_bytes);
        assert!(status.lineage_bytes <= MAX_ARTIFACT_LINEAGE_BYTES);
        assert_eq!(
            store
                .record_receipt(&artifact, &large)
                .expect_err("per-artifact cap")
                .code,
            FailureCode::ResourceExhausted
        );
        assert_eq!(
            store
                .trace(&artifact, 101)
                .expect("bounded trace")
                .receipts
                .len() as u64,
            capacity
        );

        let directory = tempfile::tempdir().expect("global-cap directory");
        let global_store = ArtifactStore::new(
            directory.path().join("private/store.sqlite"),
            64 * 1_024,
            receipt_bytes * 3,
            1_000,
        );
        global_store.initialize().expect("global-cap store");
        let artifacts = (0..8)
            .map(|index| commit(&global_store, index + 10, b"source", 100))
            .collect::<Vec<_>>();
        let global_store = Arc::new(global_store);
        let barrier = Arc::new(Barrier::new(8));
        let handles = artifacts
            .into_iter()
            .enumerate()
            .map(|(index, artifact)| {
                let store = Arc::clone(&global_store);
                let barrier = Arc::clone(&barrier);
                let mut candidate = projection_receipt(&artifact, "r".repeat(128));
                candidate.preservation.mandatory_fact_ids =
                    large.preservation.mandatory_fact_ids.clone();
                candidate.request_id = format!("{:0128x}", index + 2_000);
                thread::spawn(move || {
                    barrier.wait();
                    store.record_receipt(&artifact, &candidate)
                })
            })
            .collect::<Vec<_>>();
        let successes = handles
            .into_iter()
            .map(|handle| handle.join().expect("global writer"))
            .filter(Result::is_ok)
            .count();
        assert_eq!(successes, 3);
        assert_eq!(
            global_store
                .status(101)
                .expect("global status")
                .lineage_bytes,
            receipt_bytes * 3
        );
    }

    #[test]
    fn trace_rejects_sequence_corruption_and_gc_reclaims_lineage() {
        let (_directory, path, reference, _acquisition, _receipts) =
            legacy_v2_fixture(&receipt(), 2);
        let store = ArtifactStore::new(path.clone(), 4_096, MAX_LINEAGE_BYTES, 250);
        store.initialize().expect("migrate receipt sequence");
        let connection = Connection::open(&path).expect("sequence mutation");
        connection
            .execute_batch(
                "DROP INDEX artifact_receipts_artifact_sequence;
                 UPDATE artifact_receipts SET lineage_sequence = 0;",
            )
            .expect("duplicate sequence");
        assert_eq!(
            store
                .trace(&reference, 101)
                .expect_err("duplicate sequence")
                .code,
            FailureCode::ArtifactCorrupt
        );
        drop(connection);

        let (_directory, path, reference, _acquisition, _receipts) =
            legacy_v2_fixture(&receipt(), 2);
        let store = ArtifactStore::new(path.clone(), 4_096, MAX_LINEAGE_BYTES, 250);
        store.initialize().expect("migrate tail commitment");
        let connection = Connection::open(&path).expect("tail deletion");
        connection
            .execute(
                "DELETE FROM artifact_receipts
                 WHERE artifact_id = ?1 AND lineage_sequence = 1",
                [&reference.id],
            )
            .expect("delete lineage tail");
        assert_eq!(
            store
                .trace(&reference, 101)
                .expect_err("missing lineage tail")
                .code,
            FailureCode::ArtifactCorrupt
        );
        drop(connection);

        let (_directory, store) = fixture(4_096);
        let expired = store
            .commit(&"3".repeat(32), b"old", &receipt(), 10, 20, None)
            .expect("expired artifact");
        let live = store
            .commit(&"4".repeat(32), b"new", &receipt(), 10, 200, None)
            .expect("live artifact");
        let expired_receipt = projection_receipt(&expired, "expired".to_owned());
        let live_receipt = projection_receipt(&live, "live".to_owned());
        store
            .record_receipt(&expired, &expired_receipt)
            .expect("expired lineage");
        store
            .record_receipt(&live, &live_receipt)
            .expect("live lineage");
        let expired_bytes = serde_json::to_vec(&expired_receipt)
            .expect("expired receipt bytes")
            .len() as u64;
        let live_bytes = serde_json::to_vec(&live_receipt)
            .expect("live receipt bytes")
            .len() as u64;
        let report = store.collect_garbage(20).expect("collect lineage");
        assert_eq!(report.reclaimed_lineage_bytes, expired_bytes);
        assert_eq!(
            store.status(20).expect("post-GC status").lineage_bytes,
            live_bytes
        );
        assert_eq!(
            store.trace(&expired, 20).expect_err("expired trace").code,
            FailureCode::ArtifactExpired
        );
        assert_eq!(
            store.trace(&live, 20).expect("live trace").receipts,
            vec![live_receipt]
        );
    }

    #[test]
    fn newer_database_schema_is_rejected() {
        let directory = tempfile::tempdir().expect("temp directory");
        let private = directory.path().join("private");
        fs::create_dir(&private).expect("private directory");
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700))
            .expect("private permissions");
        let path = private.join("store.sqlite");
        let connection = Connection::open(&path).expect("connection");
        connection
            .pragma_update(None, "journal_mode", "DELETE")
            .expect("delete journal");
        connection
            .pragma_update(None, "user_version", STORE_SCHEMA_VERSION + 1)
            .expect("newer schema");
        drop(connection);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .expect("database permissions");
        let store = ArtifactStore::new(path.clone(), 1_024, MAX_LINEAGE_BYTES, 250);
        assert_eq!(
            store.initialize().expect_err("schema").code,
            FailureCode::ArtifactSchemaUnsupported
        );
        let connection = Connection::open(path).expect("inspect newer schema");
        assert_eq!(
            connection
                .pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))
                .expect("journal mode"),
            "delete"
        );
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .expect("schema version"),
            STORE_SCHEMA_VERSION + 1
        );
    }
}
