use super::*;
use crate::types::{
    AcquisitionReceipt, ByteSpan, CountUnit, Fidelity, MAX_LINEAGE_BYTES, POLICY_VERSION,
    PROJECTION_VERSION, PreservationResult, RECEIPT_SCHEMA_VERSION, SourceVariant,
};
use std::{
    fs,
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
        .commit_unvalidated(
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
            .commit_unvalidated("short", b"", &receipt(), 1, 2, None)
            .expect_err("artifact ID length")
            .code,
        FailureCode::InvariantBreach
    );
    assert_eq!(
        store
            .commit_unvalidated(&"G".repeat(32), b"", &receipt(), 1, 2, None)
            .expect_err("artifact ID alphabet")
            .code,
        FailureCode::InvariantBreach
    );
    assert_eq!(
        store
            .commit_unvalidated(&"1".repeat(32), b"", &receipt(), u64::MAX, 2, None)
            .expect_err("creation timestamp")
            .code,
        FailureCode::InvalidRequest
    );
    assert_eq!(
        store
            .commit_unvalidated(&"1".repeat(32), b"", &receipt(), 1, u64::MAX, None)
            .expect_err("expiration timestamp")
            .code,
        FailureCode::InvalidRequest
    );
    assert_eq!(
        store
            .commit_unvalidated(&"1".repeat(32), b"", &receipt(), 2, 2, None)
            .expect_err("expiration order")
            .code,
        FailureCode::InvalidRequest
    );

    let mut contradictory = receipt();
    contradictory.partial = true;
    assert_eq!(
        store
            .commit_unvalidated(&"1".repeat(32), b"", &contradictory, 1, 2, None)
            .expect_err("acquisition semantics")
            .code,
        FailureCode::InvariantBreach
    );
    let mismatched = ValidatedAcquisition::from_wire(receipt(), 0).expect("validated receipt");
    assert_eq!(
        store
            .commit(&"2".repeat(32), b"x", &mismatched, 1, 2)
            .expect_err("validated acquisition source length")
            .code,
        FailureCode::InvariantBreach
    );
    assert_eq!(
        store
            .commit_unvalidated(&"1".repeat(32), b"x", &receipt(), 1, 2, None)
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
            profile: crate::AUTO_PROFILE.to_owned(),
            applied_profile: "terminal-log/v1".to_owned(),
            mandatory_fact_ids: Vec::new(),
            aggregates: Vec::new(),
            focus_applied: false,
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
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).expect("private permissions");
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

fn downgrade_fixture_to_v1(path: &std::path::Path) {
    let connection = Connection::open(path).expect("downgrade fixture to v1");
    connection
        .execute_batch(
            "DROP TABLE artifact_receipts;
             PRAGMA user_version = 1;",
        )
        .expect("v1 schema");
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
    assert_eq!(recovered.acquisition.to_receipt(), receipt());

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
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).expect("private permissions");
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
    let sidecar = sidecar_path(&database, "-wal");
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
        .commit_unvalidated(&"1".repeat(32), b"old", &receipt(), 10, 20, None)
        .expect("expired commit");
    let live = store
        .commit_unvalidated(&"2".repeat(32), b"live", &receipt(), 10, 200, None)
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
                    .commit_unvalidated(
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
        .commit_unvalidated(&"2".repeat(32), b"x", &receipt(), 100, 200, None)
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
        .commit_unvalidated(&"3".repeat(32), b"x", &receipt(), 100, 200, None)
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
        let result = store.commit_unvalidated(&id, b"bytes", &receipt(), 100, 200, Some(fault));
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
        store
            .reference_by_id(&corrupt.id, 101)
            .expect_err("corrupt reference")
            .code,
        FailureCode::ArtifactCorrupt
    );
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
            .reference_by_id(&incomplete.id, 101)
            .expect_err("incomplete reference")
            .code,
        FailureCode::CommitFailed
    );
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
fn steady_state_open_does_not_recreate_a_missing_store() {
    let (_directory, store) = fixture(4_096);
    let store_root = store.path.parent().expect("store root");
    fs::remove_dir_all(store_root).expect("remove initialized store root");

    assert_eq!(
        store.status(101).expect_err("missing store").code,
        FailureCode::PermissionDenied
    );
    assert!(!store_root.exists());
    assert!(!store.path.exists());
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
fn contradictory_v1_migration_rolls_back_every_schema_step() {
    let contradiction = AcquisitionReceipt {
        partial: true,
        ..receipt()
    };
    let (_directory, path, _reference, acquisition_json, _receipt_blobs) =
        legacy_v2_fixture(&contradiction, 0);
    downgrade_fixture_to_v1(&path);

    let store = ArtifactStore::new(path.clone(), 4_096, MAX_LINEAGE_BYTES, 250);
    assert_eq!(
        store
            .initialize()
            .expect_err("contradictory migration")
            .code,
        FailureCode::ArtifactCorrupt
    );

    let connection = Connection::open(&path).expect("inspect rolled-back v1 store");
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .expect("schema version"),
        1
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'artifact_receipts'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("receipt table count"),
        0
    );
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
fn v1_migration_commits_complete_v3_state_atomically() {
    let (_directory, path, reference, acquisition_json, _receipt_blobs) =
        legacy_v2_fixture(&receipt(), 0);
    downgrade_fixture_to_v1(&path);

    let store = ArtifactStore::new(path.clone(), 4_096, MAX_LINEAGE_BYTES, 250);
    store.initialize().expect("migrate v1 to v3");

    let connection = Connection::open(&path).expect("inspect migrated v1 store");
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .expect("schema version"),
        STORE_SCHEMA_VERSION
    );
    let (digest, count, bytes, head): (String, i64, i64, String) = connection
        .query_row(
            "SELECT source_metadata_sha256, lineage_count, lineage_bytes,
                    lineage_head_sha256
             FROM artifacts WHERE id = ?1",
            [&reference.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("migrated artifact state");
    assert_eq!(digest, sha256_hex(&acquisition_json));
    assert_eq!((count, bytes, head.as_str()), (0, 0, EMPTY_LINEAGE_SHA256));
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM artifact_receipts", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("receipt count"),
        0
    );
    drop(connection);
    assert_eq!(
        store.retrieve(&reference, 101).expect("retrieve").bytes,
        b"legacy"
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

    let (_directory, path, _reference, _acquisition, _receipts) = legacy_v2_fixture(&receipt(), 1);
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

#[path = "tests/lineage.rs"]
mod lineage;
