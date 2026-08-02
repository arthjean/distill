use super::{
    StoredArtifact,
    integrity::{nonnegative_u64, sha256_hex, verify_exact_digest},
    sqlite_errors::map_read_error,
};
use crate::types::{
    ARTIFACT_SCHEMA_VERSION, AcquisitionReceipt, ArtifactRef, Failure, FailureCode,
    ValidatedAcquisition,
};
use rusqlite::{Connection, OptionalExtension};

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

pub(super) struct VerifiedArtifact {
    pub reference: ArtifactRef,
    pub stored: StoredArtifact,
}

pub(super) fn read_artifact(
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

pub(super) fn load_verified_artifact(
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
        return Err(missing_artifact_failure(connection, id, now)?);
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
