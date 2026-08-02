use super::{
    ArtifactStore, EMPTY_LINEAGE_SHA256, StoredTrace, nonnegative_u64, read_artifact, sha256_hex,
};
use super::{
    lineage::{
        lineage_chain_sha256, lineage_usage, validate_receipt, verify_exact_digest, verify_lineage,
    },
    permissions::enforce_store_modes,
    sqlite_errors::{map_read_error, map_write_error},
};
use crate::types::{ArtifactRef, Failure, FailureCode, MAX_ARTIFACT_LINEAGE_BYTES, Receipt};
use rusqlite::{OptionalExtension, TransactionBehavior, params};

struct ReceiptTargetRow {
    schema: String,
    digest: String,
    source_bytes: i64,
    created_at: i64,
    expires_at: i64,
    lineage_count: i64,
    lineage_bytes: i64,
    lineage_head: String,
}

struct LineageClaimRow {
    count: i64,
    bytes: i64,
    head: String,
}

struct ReceiptLineageRow {
    sequence: i64,
    request_id: String,
    receipt_json: Vec<u8>,
    receipt_digest: String,
    chain: String,
}

impl ArtifactStore {
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
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_write_error)?;
        let target: Option<ReceiptTargetRow> = transaction
            .query_row(
                "SELECT schema_version, sha256, source_bytes, created_at, expires_at,
                        lineage_count, lineage_bytes, lineage_head_sha256
                 FROM artifacts WHERE id = ?1 AND state = 'committed'",
                [&reference.id],
                |row| {
                    Ok(ReceiptTargetRow {
                        schema: row.get(0)?,
                        digest: row.get(1)?,
                        source_bytes: row.get(2)?,
                        created_at: row.get(3)?,
                        expires_at: row.get(4)?,
                        lineage_count: row.get(5)?,
                        lineage_bytes: row.get(6)?,
                        lineage_head: row.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(map_read_error)?;
        let Some(target) = target else {
            return Err(Failure::new(
                FailureCode::ArtifactCorrupt,
                "artifact receipt target failed integrity validation",
            )
            .with_artifact(reference.clone()));
        };
        if target.schema != reference.schema_version
            || target.digest != reference.source_sha256
            || nonnegative_u64(target.source_bytes, "source byte count")? != reference.source_bytes
            || nonnegative_u64(target.created_at, "creation time")? != reference.created_at
            || nonnegative_u64(target.expires_at, "expiration time")? != reference.expires_at
        {
            return Err(Failure::new(
                FailureCode::ArtifactCorrupt,
                "artifact receipt target failed integrity validation",
            )
            .with_artifact(reference.clone()));
        }
        let global_lineage = lineage_usage(&transaction)?;
        let lineage = verify_lineage(
            &transaction,
            reference,
            target.lineage_count,
            target.lineage_bytes,
            target.lineage_head,
        )?;
        let next_global = global_lineage.checked_add(receipt_bytes);
        let next_artifact = lineage.bytes.checked_add(receipt_bytes);
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
        let lineage_sequence = i64::try_from(lineage.count).map_err(|_| {
            Failure::new(
                FailureCode::ResourceExhausted,
                "artifact receipt sequence is exhausted",
            )
        })?;
        let lineage_chain = lineage_chain_sha256(&lineage.head, lineage.count, &receipt_json);
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
        let next_count = lineage.count.checked_add(1).ok_or_else(|| {
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
        let mut connection = self.connect()?;
        let transaction = connection.transaction().map_err(map_read_error)?;
        let stored = read_artifact(&transaction, reference, now)?;
        let claim: LineageClaimRow = transaction
            .query_row(
                "SELECT lineage_count, lineage_bytes, lineage_head_sha256
                 FROM artifacts WHERE id = ?1",
                [&reference.id],
                |row| {
                    Ok(LineageClaimRow {
                        count: row.get(0)?,
                        bytes: row.get(1)?,
                        head: row.get(2)?,
                    })
                },
            )
            .map_err(map_read_error)?;
        let lineage = verify_lineage(
            &transaction,
            reference,
            claim.count,
            claim.bytes,
            claim.head,
        )?;
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
            let row = ReceiptLineageRow {
                sequence: row.get(0).map_err(map_read_error)?,
                request_id: row.get(1).map_err(map_read_error)?,
                receipt_json: row.get(2).map_err(map_read_error)?,
                receipt_digest: row.get(3).map_err(map_read_error)?,
                chain: row.get(4).map_err(map_read_error)?,
            };
            let sequence = nonnegative_u64(row.sequence, "receipt lineage sequence")?;
            if sequence != expected_sequence {
                return Err(Failure::new(
                    FailureCode::ArtifactCorrupt,
                    "artifact receipt lineage sequence is corrupt",
                )
                .with_artifact(reference.clone()));
            }
            expected_sequence = expected_sequence.saturating_add(1);
            verify_exact_digest(
                &row.receipt_json,
                &row.receipt_digest,
                "artifact projection receipt integrity verification failed",
                reference,
            )?;
            lineage_bytes = lineage_bytes.saturating_add(row.receipt_json.len() as u64);
            if lineage_bytes > MAX_ARTIFACT_LINEAGE_BYTES {
                return Err(Failure::new(
                    FailureCode::ArtifactCorrupt,
                    "artifact receipt lineage exceeds its logical cap",
                )
                .with_artifact(reference.clone()));
            }
            let expected_chain = lineage_chain_sha256(&lineage_head, sequence, &row.receipt_json);
            if row.chain != expected_chain {
                return Err(Failure::new(
                    FailureCode::ArtifactCorrupt,
                    "artifact receipt lineage commitment is corrupt",
                )
                .with_artifact(reference.clone()));
            }
            lineage_head = expected_chain;
            let receipt: Receipt = serde_json::from_slice(&row.receipt_json).map_err(|_| {
                Failure::new(
                    FailureCode::ArtifactCorrupt,
                    "artifact projection receipt is corrupt",
                )
                .with_artifact(reference.clone())
            })?;
            if receipt.request_id != row.request_id {
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
        if lineage_bytes != lineage.bytes
            || expected_sequence != lineage.count
            || lineage_head != lineage.head
        {
            return Err(Failure::new(
                FailureCode::ArtifactCorrupt,
                "artifact receipt lineage commitment is corrupt",
            )
            .with_artifact(reference.clone()));
        }
        transaction.commit().map_err(map_read_error)?;
        Ok(StoredTrace {
            acquisition: stored.acquisition.into_receipt(),
            receipts,
        })
    }
}
