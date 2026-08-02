use super::sqlite_errors::map_write_error;
use super::{GcReport, TOMBSTONE_TTL_SECONDS, nonnegative_u64};
use crate::types::{Failure, FailureCode};
use rusqlite::{Transaction, params};

struct GarbageCollectionRow {
    records: i64,
    bytes: i64,
}

pub(super) fn store_usage(transaction: &Transaction<'_>) -> Result<u64, Failure> {
    let used: i64 = transaction
        .query_row(
            "SELECT COALESCE(SUM(source_bytes), 0) FROM artifacts",
            [],
            |row| row.get(0),
        )
        .map_err(map_write_error)?;
    nonnegative_u64(used, "store usage")
}

pub(super) fn collect_in_transaction(
    transaction: &Transaction<'_>,
    now: u64,
) -> Result<GcReport, Failure> {
    let now_sql = i64::try_from(now).map_err(|_| {
        Failure::new(
            FailureCode::InvalidRequest,
            "garbage collection time exceeds SQLite limits",
        )
    })?;
    let row: GarbageCollectionRow = transaction
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(source_bytes), 0)
             FROM artifacts WHERE expires_at <= ?1",
            [now_sql],
            |row| {
                Ok(GarbageCollectionRow {
                    records: row.get(0)?,
                    bytes: row.get(1)?,
                })
            },
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
        reclaimed_bytes: nonnegative_u64(row.bytes, "garbage collection bytes")?,
        reclaimed_lineage_bytes: nonnegative_u64(
            lineage_bytes,
            "garbage collection lineage bytes",
        )?,
        reclaimed_records: nonnegative_u64(row.records, "garbage collection records")?,
    })
}
