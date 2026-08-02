use super::{ArtifactStore, STORE_SCHEMA_VERSION, lineage::validate_lineage_bounds_connection};
use super::{migration::migrate_v2_to_v3, permissions, sqlite_errors::map_open_error};
use crate::types::{Failure, FailureCode};
use rusqlite::{Connection, OpenFlags};
use std::time::Duration;

impl ArtifactStore {
    pub(super) fn open(&self, check_integrity: bool) -> Result<Connection, Failure> {
        let parent = self.path.parent().ok_or_else(|| {
            Failure::new(
                FailureCode::UnsafeRoot,
                "artifact store path has no parent directory",
            )
        })?;
        permissions::secure_store_root(parent)?;
        permissions::validate_optional_store_file(&self.path)?;
        for suffix in ["-wal", "-shm"] {
            permissions::validate_optional_store_file(&permissions::sidecar_path(
                &self.path, suffix,
            ))?;
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
        match schema_version {
            0 => create_schema(&connection)?,
            1 => {
                create_v2_receipts(&connection)?;
                migrate_v2_to_v3(&mut connection, self.max_lineage_bytes)?;
            }
            2 => migrate_v2_to_v3(&mut connection, self.max_lineage_bytes)?,
            _ => {}
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
        permissions::enforce_store_modes(&self.path)?;
        Ok(connection)
    }
}

fn create_schema(connection: &Connection) -> Result<(), Failure> {
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
        .map_err(map_open_error)
}

fn create_v2_receipts(connection: &Connection) -> Result<(), Failure> {
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
        .map_err(map_open_error)
}
