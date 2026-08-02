use super::{
    EMPTY_LINEAGE_SHA256, lineage_chain_sha256, lineage_usage, map_read_error, map_write_error,
    nonnegative_u64, sha256_hex, validate_receipt,
};
use crate::types::{
    ARTIFACT_SCHEMA_VERSION, AcquisitionReceipt, ArtifactRef, Failure, FailureCode,
    MAX_ARTIFACT_LINEAGE_BYTES, Receipt,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

#[derive(Debug)]
struct MigratedLineage {
    count: u64,
    bytes: u64,
    head: String,
}

#[derive(Debug)]
struct LegacyArtifactRow {
    id: String,
    schema: String,
    state: String,
    source: Vec<u8>,
    source_digest: String,
    source_bytes: i64,
    acquisition_json: Vec<u8>,
    created_at: i64,
    expires_at: i64,
}

struct LegacyReceiptRow {
    sequence: i64,
    artifact_id: String,
    request_id: String,
    receipt_json: Vec<u8>,
}

pub(super) fn migrate_v2_to_v3(
    connection: &mut Connection,
    max_lineage_bytes: u64,
) -> Result<(), Failure> {
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
                Ok(LegacyArtifactRow {
                    id: row.get(0)?,
                    schema: row.get(1)?,
                    state: row.get(2)?,
                    source: row.get(3)?,
                    source_digest: row.get(4)?,
                    source_bytes: row.get(5)?,
                    acquisition_json: row.get(6)?,
                    created_at: row.get(7)?,
                    expires_at: row.get(8)?,
                })
            })
            .map_err(map_read_error)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(map_read_error)?
    };
    let mut references = std::collections::BTreeMap::new();
    for row in artifacts {
        let source_bytes = nonnegative_u64(row.source_bytes, "source byte count")?;
        let created_at = nonnegative_u64(row.created_at, "creation time")?;
        let expires_at = nonnegative_u64(row.expires_at, "expiration time")?;
        let reference = ArtifactRef {
            schema_version: row.schema,
            id: row.id.clone(),
            source_sha256: row.source_digest.clone(),
            source_bytes,
            created_at,
            expires_at,
        };
        if reference.schema_version != ARTIFACT_SCHEMA_VERSION
            || row.state != "committed"
            || row.source.len() as u64 != source_bytes
            || sha256_hex(&row.source) != row.source_digest
        {
            return Err(
                Failure::new(FailureCode::ArtifactCorrupt, "v2 artifact state is corrupt")
                    .with_artifact(reference),
            );
        }
        let acquisition: AcquisitionReceipt = serde_json::from_slice(&row.acquisition_json)
            .map_err(|_| {
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
                params![sha256_hex(&row.acquisition_json), row.id],
            )
            .map_err(map_write_error)?;
        references.insert(row.id, reference);
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
                Ok(LegacyReceiptRow {
                    sequence: row.get(0)?,
                    artifact_id: row.get(1)?,
                    request_id: row.get(2)?,
                    receipt_json: row.get(3)?,
                })
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
    for row in receipt_rows {
        let reference = references.get(&row.artifact_id).ok_or_else(|| {
            Failure::new(
                FailureCode::ArtifactCorrupt,
                "v2 receipt names a missing artifact",
            )
        })?;
        let receipt: Receipt = serde_json::from_slice(&row.receipt_json).map_err(|_| {
            Failure::new(
                FailureCode::ArtifactCorrupt,
                "v2 projection receipt is corrupt",
            )
            .with_artifact(reference.clone())
        })?;
        if receipt.request_id != row.request_id {
            return Err(Failure::new(
                FailureCode::ArtifactCorrupt,
                "v2 receipt request binding is corrupt",
            )
            .with_artifact(reference.clone()));
        }
        validate_receipt(&receipt, reference, FailureCode::ArtifactCorrupt)?;
        let lineage = lineage.get_mut(&row.artifact_id).ok_or_else(|| {
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
        let chain = lineage_chain_sha256(&lineage.head, lineage.count, &row.receipt_json);
        transaction
            .execute(
                "UPDATE artifact_receipts
                 SET lineage_sequence = ?1, receipt_metadata_sha256 = ?2,
                     lineage_chain_sha256 = ?3
                 WHERE sequence = ?4",
                params![
                    sequence_sql,
                    sha256_hex(&row.receipt_json),
                    chain,
                    row.sequence
                ],
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
            .checked_add(row.receipt_json.len() as u64)
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
