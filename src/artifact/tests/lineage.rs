use super::*;

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
            .reference_by_id(&digest_corrupt.id, 101)
            .expect_err("acquisition digest")
            .code,
        FailureCode::ArtifactCorrupt
    );
    assert_eq!(
        store
            .retrieve(&digest_corrupt, 101)
            .expect_err("acquisition digest retrieval")
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
            .reference_by_id(&semantic_corrupt.id, 101)
            .expect_err("acquisition semantics")
            .code,
        FailureCode::ArtifactCorrupt
    );
    assert_eq!(
        store
            .retrieve(&semantic_corrupt, 101)
            .expect_err("acquisition semantics retrieval")
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
            .commit_unvalidated(&"5".repeat(32), b"source", &contradiction, 100, 200, None,)
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
    let (_directory, path, reference, _acquisition, _receipts) = legacy_v2_fixture(&receipt(), 2);
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

    let (_directory, path, reference, _acquisition, _receipts) = legacy_v2_fixture(&receipt(), 2);
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
        .commit_unvalidated(&"3".repeat(32), b"old", &receipt(), 10, 20, None)
        .expect("expired artifact");
    let live = store
        .commit_unvalidated(&"4".repeat(32), b"new", &receipt(), 10, 200, None)
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
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).expect("private permissions");
    let path = private.join("store.sqlite");
    let connection = Connection::open(&path).expect("connection");
    connection
        .pragma_update(None, "journal_mode", "DELETE")
        .expect("delete journal");
    connection
        .pragma_update(None, "user_version", STORE_SCHEMA_VERSION + 1)
        .expect("newer schema");
    drop(connection);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("database permissions");
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
