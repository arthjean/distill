use distill::{ArtifactRef, Budget, CountUnit, Failure, FailureCode, Request, Retention, Source};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{self, Write},
};
use tiktoken_rs::cl100k_base_singleton;

mod envelope;

pub(crate) use envelope::{codex_error_envelope, mcp_error_envelope, projection_envelope};

pub(crate) const BROKEN_PIPE_EXIT: i32 = 74;
pub(crate) const DEFAULT_PRESERVATION_PROFILE: &str = "plain-text/v1";

pub(crate) fn budget_for(unit: CountUnit, total: u64, reserved: u64) -> Budget {
    Budget {
        unit,
        total_visible_limit: total,
        reserved_envelope: reserved,
        token_profile: (unit == CountUnit::Tokens).then(|| distill::CL100K_PROFILE.to_owned()),
    }
}

pub(crate) fn default_request(request_id: String, source: Source, budget: Budget) -> Request {
    Request {
        contract_version: distill::CONTRACT_VERSION.to_owned(),
        request_id,
        source,
        budget,
        preservation_profile: DEFAULT_PRESERVATION_PROFILE.to_owned(),
        retention: Retention::default(),
    }
}

pub(crate) fn count_visible(text: &str, unit: CountUnit) -> u64 {
    match unit {
        CountUnit::Bytes => text.len() as u64,
        CountUnit::Tokens => cl100k_base_singleton().encode_ordinary(text).len() as u64,
    }
}

pub(crate) fn normalize_root_relative(path: String) -> String {
    if path == "." { String::new() } else { path }
}

pub(crate) fn adapter_failure(
    code: FailureCode,
    message: &str,
    artifact: Option<ArtifactRef>,
) -> Failure {
    Failure {
        code,
        safe_message: message.to_owned(),
        request_id: None,
        details: BTreeMap::new(),
        artifact,
        acquisition: None,
    }
}

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
