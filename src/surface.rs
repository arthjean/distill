use distill::{ArtifactRef, Failure, FailureCode};
use serde_json::Value;
use std::io::{self, Write};

pub(crate) const BROKEN_PIPE_EXIT: i32 = 74;

pub(crate) fn bounded_correlation_id(mut value: String) -> String {
    let limit = distill::MAX_IDENTIFIER_BYTES;
    if value.len() <= limit {
        return value;
    }
    let mut boundary = limit;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value
}

#[derive(Debug)]
pub(crate) struct SurfaceError {
    pub(crate) exit_code: i32,
    pub(crate) code: String,
    pub(crate) message: String,
    pub(crate) artifact: Option<ArtifactRef>,
}

impl SurfaceError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self {
            exit_code: 2,
            code: "invalid_input".to_owned(),
            message: message.into(),
            artifact: None,
        }
    }

    pub(crate) fn output(error: io::Error) -> Self {
        let broken_pipe = error.kind() == io::ErrorKind::BrokenPipe;
        Self {
            exit_code: if broken_pipe { BROKEN_PIPE_EXIT } else { 70 },
            code: if broken_pipe {
                "broken_pipe".to_owned()
            } else {
                "output_failure".to_owned()
            },
            message: if broken_pipe {
                "output consumer closed the pipe".to_owned()
            } else {
                "cannot write command output".to_owned()
            },
            artifact: None,
        }
    }
}

impl From<Failure> for SurfaceError {
    fn from(failure: Failure) -> Self {
        let exit_code = match failure.code {
            FailureCode::InvalidRequest
            | FailureCode::SchemaUnsupported
            | FailureCode::SourceUnsupported
            | FailureCode::TokenProfileUnsupported
            | FailureCode::InputTooLarge
            | FailureCode::ResourceExhausted
            | FailureCode::UnsafeRoot => 2,
            FailureCode::BudgetUnsatisfiable => 3,
            FailureCode::ArtifactUnknown
            | FailureCode::ArtifactExpired
            | FailureCode::ArtifactCorrupt
            | FailureCode::ArtifactSchemaUnsupported => 4,
            FailureCode::PermissionDenied
            | FailureCode::StoreFull
            | FailureCode::StoreBusy
            | FailureCode::CommitFailed => 5,
            FailureCode::AcquisitionFailed => 6,
            FailureCode::InvariantBreach => 70,
        };
        Self {
            exit_code,
            code: failure.code.as_str().to_owned(),
            message: failure.safe_message,
            artifact: failure.artifact,
        }
    }
}

pub(crate) fn write_json_line<W: Write>(output: &mut W, value: &Value) -> Result<(), SurfaceError> {
    serde_json::to_writer(&mut *output, value)
        .map_err(|error| SurfaceError::output(io::Error::other(error)))?;
    output.write_all(b"\n").map_err(SurfaceError::output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn correlation_ids_are_bounded_on_utf8_boundaries() {
        let value = format!("{}é", "a".repeat(127));
        let bounded = bounded_correlation_id(value);
        assert_eq!(bounded.len(), distill::MAX_IDENTIFIER_BYTES - 1);
        assert!(bounded.is_char_boundary(bounded.len()));
    }
}
