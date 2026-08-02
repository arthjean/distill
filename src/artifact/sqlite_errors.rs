use crate::types::{Failure, FailureCode};
use rusqlite::ErrorCode;

pub(super) fn map_open_error(error: rusqlite::Error) -> Failure {
    map_error(&error, Operation::Open)
}

pub(super) fn map_write_error(error: rusqlite::Error) -> Failure {
    map_error(&error, Operation::Write)
}

pub(super) fn map_read_error(error: rusqlite::Error) -> Failure {
    map_error(&error, Operation::Read)
}

#[derive(Clone, Copy)]
enum Operation {
    Open,
    Read,
    Write,
}

#[derive(Clone, Copy)]
enum ErrorClass {
    Busy,
    Corrupt,
    Full,
    Permission,
    Other,
}

fn map_error(error: &rusqlite::Error, operation: Operation) -> Failure {
    match (operation, classify(error)) {
        (_, ErrorClass::Corrupt) => {
            Failure::new(FailureCode::ArtifactCorrupt, "artifact database is corrupt")
        }
        (Operation::Open | Operation::Read, ErrorClass::Busy) => {
            Failure::new(FailureCode::StoreBusy, "artifact store is busy")
        }
        (Operation::Write, ErrorClass::Busy) => {
            Failure::new(FailureCode::StoreBusy, "artifact transaction timed out")
        }
        (Operation::Open, ErrorClass::Permission) => Failure::new(
            FailureCode::PermissionDenied,
            "artifact store is not writable",
        ),
        (Operation::Write, ErrorClass::Permission) => Failure::new(
            FailureCode::PermissionDenied,
            "artifact transaction is not writable",
        ),
        (Operation::Open | Operation::Write, ErrorClass::Full) => {
            Failure::new(FailureCode::StoreFull, "artifact store is full")
        }
        (Operation::Open, ErrorClass::Other) => Failure::new(
            FailureCode::CommitFailed,
            "artifact store initialization failed",
        ),
        (Operation::Write, ErrorClass::Other) => {
            Failure::new(FailureCode::CommitFailed, "artifact transaction failed")
        }
        (Operation::Read, _) => Failure::new(
            FailureCode::ArtifactCorrupt,
            "artifact record cannot be decoded",
        ),
    }
}

fn classify(error: &rusqlite::Error) -> ErrorClass {
    let rusqlite::Error::SqliteFailure(code, _) = error else {
        return ErrorClass::Other;
    };
    match code.code {
        ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => ErrorClass::Busy,
        ErrorCode::PermissionDenied | ErrorCode::ReadOnly => ErrorClass::Permission,
        ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase => ErrorClass::Corrupt,
        ErrorCode::DiskFull => ErrorClass::Full,
        _ => ErrorClass::Other,
    }
}
