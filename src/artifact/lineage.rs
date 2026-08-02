use super::{
    LINEAGE_DIGEST_DOMAIN, MAX_RECEIPT_SPANS, map_read_error, map_write_error, nonnegative_u64,
    sha256_hex,
};
use crate::types::{
    ArtifactRef, CL100K_PROFILE, CountUnit, Failure, FailureCode, Fidelity,
    MAX_ARTIFACT_LINEAGE_BYTES, POLICY_VERSION, PROJECTION_VERSION, RECEIPT_SCHEMA_VERSION,
    Receipt,
};
use rusqlite::{Connection, OptionalExtension, Transaction};
use sha2::{Digest, Sha256};

struct LineageShapeRow {
    count: i64,
    bytes: i64,
    minimum: i64,
    maximum: i64,
    distinct: i64,
}

#[derive(Debug)]
pub(super) struct LineageShape {
    pub count: u64,
    pub bytes: u64,
    pub last_chain: Option<String>,
}

#[derive(Debug)]
pub(super) struct VerifiedLineage {
    pub count: u64,
    pub bytes: u64,
    pub head: String,
}

pub(super) fn verify_lineage(
    connection: &Connection,
    reference: &ArtifactRef,
    claimed_count: i64,
    claimed_bytes: i64,
    claimed_head: String,
) -> Result<VerifiedLineage, Failure> {
    let claimed_count = nonnegative_u64(claimed_count, "claimed receipt count")?;
    let claimed_bytes = nonnegative_u64(claimed_bytes, "claimed lineage usage")?;
    let shape = lineage_shape(connection, reference)?;
    if shape.count != claimed_count
        || shape.bytes != claimed_bytes
        || shape.bytes > MAX_ARTIFACT_LINEAGE_BYTES
        || !valid_sha256(&claimed_head)
        || (shape.count == 0 && claimed_head != super::EMPTY_LINEAGE_SHA256)
        || (shape.count > 0 && shape.last_chain.as_deref() != Some(claimed_head.as_str()))
    {
        return Err(Failure::new(
            FailureCode::ArtifactCorrupt,
            "artifact receipt lineage commitment is corrupt",
        )
        .with_artifact(reference.clone()));
    }
    Ok(VerifiedLineage {
        count: shape.count,
        bytes: shape.bytes,
        head: claimed_head,
    })
}

pub(super) fn lineage_shape(
    connection: &Connection,
    reference: &ArtifactRef,
) -> Result<LineageShape, Failure> {
    let row: LineageShapeRow = connection
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(length(receipt_metadata)), 0),
                    COALESCE(MIN(lineage_sequence), -1),
                    COALESCE(MAX(lineage_sequence), -1),
                    COUNT(DISTINCT lineage_sequence)
             FROM artifact_receipts WHERE artifact_id = ?1",
            [&reference.id],
            |row| {
                Ok(LineageShapeRow {
                    count: row.get(0)?,
                    bytes: row.get(1)?,
                    minimum: row.get(2)?,
                    maximum: row.get(3)?,
                    distinct: row.get(4)?,
                })
            },
        )
        .map_err(map_read_error)?;
    let count = nonnegative_u64(row.count, "receipt row count")?;
    let bytes = nonnegative_u64(row.bytes, "artifact lineage usage")?;
    let distinct = nonnegative_u64(row.distinct, "distinct receipt sequence count")?;
    let expected_max = i64::try_from(count)
        .ok()
        .and_then(|count| count.checked_sub(1))
        .unwrap_or(i64::MAX);
    if distinct != count
        || (count == 0 && (row.minimum != -1 || row.maximum != -1))
        || (count > 0 && (row.minimum != 0 || row.maximum != expected_max))
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

pub(super) fn validate_lineage_bounds_connection(
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

pub(super) fn lineage_usage(transaction: &Transaction<'_>) -> Result<u64, Failure> {
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

pub(super) fn lineage_usage_connection(connection: &Connection) -> Result<u64, Failure> {
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

pub(super) fn verify_exact_digest(
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

pub(super) fn valid_sha256(digest: &str) -> bool {
    digest.len() == 64 && digest.bytes().all(crate::contract::is_lower_hex)
}

pub(super) fn lineage_chain_sha256(previous: &str, sequence: u64, receipt_json: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(LINEAGE_DIGEST_DOMAIN);
    digest.update(previous.as_bytes());
    digest.update(sequence.to_be_bytes());
    digest.update(receipt_json);
    format!("{:x}", digest.finalize())
}

pub(super) fn validate_receipt(
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
        || receipt.request_id.len() > crate::contract::MAX_IDENTIFIER_BYTES
        || receipt.preservation.profile.is_empty()
        || receipt.preservation.profile.len() > crate::contract::MAX_IDENTIFIER_BYTES
        || receipt.preservation.mandatory_fact_ids.len() > 256
        || receipt
            .preservation
            .mandatory_fact_ids
            .iter()
            .any(|id| id.is_empty() || id.len() > crate::contract::MAX_IDENTIFIER_BYTES)
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
