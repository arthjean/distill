use crate::types::{
    ARTIFACT_SCHEMA_VERSION, AcquisitionReceipt, ArtifactRef, Failure, FailureCode,
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

const STORE_SCHEMA_VERSION: i64 = 1;
const TOMBSTONE_TTL_SECONDS: u64 = 30 * 24 * 60 * 60;

#[derive(Clone, Debug)]
pub(crate) struct ArtifactStore {
    path: PathBuf,
    max_bytes: u64,
    busy_timeout_ms: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct StoredArtifact {
    pub bytes: Vec<u8>,
    pub acquisition: AcquisitionReceipt,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GcReport {
    pub reclaimed_bytes: u64,
    pub reclaimed_records: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommitFault {
    BeforeInsert,
    BeforeCommit,
    AfterCommit,
    BeforeReadback,
}

impl ArtifactStore {
    pub(crate) fn new(path: PathBuf, max_bytes: u64, busy_timeout_ms: u64) -> Self {
        Self {
            path,
            max_bytes,
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
        let digest = sha256_hex(bytes);
        let acquisition_json = serde_json::to_vec(acquisition).map_err(|_| {
            Failure::new(
                FailureCode::InvariantBreach,
                "acquisition metadata cannot be serialized",
            )
        })?;
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
                  source_metadata, created_at, expires_at)
                 VALUES (?1, ?2, 'committed', ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    id,
                    ARTIFACT_SCHEMA_VERSION,
                    bytes,
                    digest,
                    source_bytes_sql,
                    acquisition_json,
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
        let (stored_digest, stored_bytes, state): (String, Vec<u8>, String) = connection
            .query_row(
                "SELECT sha256, source, state FROM artifacts WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(map_read_error)?;
        if state != "committed"
            || stored_digest != digest
            || stored_bytes.len() != bytes.len()
            || sha256_hex(&stored_bytes) != digest
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
        if reference.schema_version != ARTIFACT_SCHEMA_VERSION {
            return Err(Failure::new(
                FailureCode::ArtifactSchemaUnsupported,
                "artifact reference schema is unsupported",
            ));
        }
        let connection = self.open(false)?;
        let record = connection
            .query_row(
                "SELECT schema_version, state, source, sha256, source_bytes,
                        source_metadata, created_at, expires_at
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
                        row.get::<_, i64>(6)?,
                        row.get::<_, i64>(7)?,
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
            created_at,
            expires_at,
        )) = record
        else {
            return self.missing_artifact(&connection, &reference.id, now);
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
            drop(connection);
            let _report = self.collect_garbage(now)?;
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
        let acquisition = serde_json::from_slice(&acquisition_json).map_err(|_| {
            Failure::new(
                FailureCode::ArtifactCorrupt,
                "artifact acquisition metadata is corrupt",
            )
            .with_artifact(stored_reference.clone())
        })?;
        Ok(StoredArtifact { bytes, acquisition })
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
        &self,
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
        let connection = Connection::open_with_flags(
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
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(map_open_error)?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(map_open_error)?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
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
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS artifacts (
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
                CREATE INDEX IF NOT EXISTS artifacts_expiry
                    ON artifacts(expires_at);
                CREATE TABLE IF NOT EXISTS tombstones (
                    id TEXT PRIMARY KEY,
                    schema_version TEXT NOT NULL,
                    expired_at INTEGER NOT NULL,
                    purge_at INTEGER NOT NULL,
                    failure_code TEXT NOT NULL
                );",
            )
            .map_err(map_open_error)?;
        if schema_version == 0 {
            connection
                .pragma_update(None, "user_version", STORE_SCHEMA_VERSION)
                .map_err(map_open_error)?;
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
        }
        enforce_store_modes(&self.path)?;
        Ok(connection)
    }
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
        reclaimed_records: nonnegative_u64(records, "garbage collection records")?,
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
    use crate::types::{AcquisitionReceipt, SourceVariant};
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
    fn restart_recovery_is_byte_exact_and_private() {
        let (_directory, store) = fixture(1_024);
        let artifact = commit(&store, 1, b"\0source\xff", 100);
        let restarted = ArtifactStore::new(store.path.clone(), 1_024, 250);
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
        let store = ArtifactStore::new(database.clone(), 1_024, 250);
        assert_eq!(
            store.initialize().expect_err("database symlink").code,
            FailureCode::UnsafeRoot
        );

        fs::remove_file(&database).expect("remove database symlink");
        store.initialize().expect("initialize real database");
        let sidecar = PathBuf::from(format!("{}-wal", database.display()));
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
        let contender = ArtifactStore::new(store.path.clone(), 1_024, 1);
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
        let corrupt_store = ArtifactStore::new(corrupt_path, 4_096, 10);
        assert_eq!(
            corrupt_store.initialize().expect_err("database").code,
            FailureCode::ArtifactCorrupt
        );
    }

    #[test]
    fn newer_database_schema_is_rejected() {
        let (_directory, store) = fixture(1_024);
        let connection = Connection::open(&store.path).expect("connection");
        connection
            .pragma_update(None, "user_version", STORE_SCHEMA_VERSION + 1)
            .expect("newer schema");
        drop(connection);
        assert_eq!(
            store.initialize().expect_err("schema").code,
            FailureCode::ArtifactSchemaUnsupported
        );
    }
}
