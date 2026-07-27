use crate::types::{
    ByteString, CONTRACT_VERSION, EngineConfig, Failure, FailureCode, Request, Retention, Source,
};

pub const MAX_IDENTIFIER_BYTES: usize = 128;
pub const MAX_PATH_BYTES: usize = 4_096;
pub const MAX_PROCESS_ARGUMENTS: usize = 4_096;
pub const MAX_PROCESS_ARGUMENT_BYTES: usize = 1024 * 1024;
pub const MAX_PROCESS_EXECUTABLE_BYTES: usize = 4_096;
pub const MAX_SOURCE_BYTES: usize = 10 * 1024 * 1024;
pub const MIN_PROCESS_TIMEOUT_MS: u64 = 100;
pub const MAX_PROCESS_TIMEOUT_MS: u64 = 300_000;
pub(crate) const DEFAULT_PROCESS_TIMEOUT_MS: u64 = 30_000;

pub(crate) fn validate(request: &Request, config: &EngineConfig) -> Result<(), Failure> {
    if request.contract_version != CONTRACT_VERSION {
        return Err(correlate(
            request,
            Failure::new(
                FailureCode::SchemaUnsupported,
                "request contract version is unsupported",
            ),
        ));
    }
    if !valid_correlation_id(&request.request_id) {
        return Err(Failure::new(
            FailureCode::InvalidRequest,
            "request ID is missing or too long",
        ));
    }

    let invalid = |message| {
        Failure::new(FailureCode::InvalidRequest, message).for_request(&request.request_id)
    };
    match &request.source {
        Source::Inline { bytes, .. } if bytes.0.len() > MAX_SOURCE_BYTES => {
            return Err(Failure::new(
                FailureCode::InputTooLarge,
                "inline source exceeds the 10 MiB limit",
            )
            .for_request(&request.request_id));
        }
        Source::File {
            root_id,
            relative_path,
            ..
        } => {
            if !valid_identifier(root_id)
                || relative_path.0.len() > MAX_PATH_BYTES
                || relative_path.0.contains(&0)
            {
                return Err(invalid(
                    "file source metadata is missing or exceeds its bounds",
                ));
            }
            if !config.roots.contains_key(root_id) {
                return Err(Failure::new(
                    FailureCode::UnsafeRoot,
                    "file source names an unknown root",
                )
                .for_request(&request.request_id));
            }
        }
        Source::Process {
            executable,
            argv,
            cwd_root_id,
            cwd_relative_path,
            timeout_ms,
            environment_profile,
        } => {
            validate_process(executable, argv, *timeout_ms)
                .map_err(|failure| failure.for_request(&request.request_id))?;
            if !valid_identifier(cwd_root_id)
                || cwd_relative_path.0.len() > MAX_PATH_BYTES
                || cwd_relative_path.0.contains(&0)
            {
                return Err(invalid(
                    "process source metadata is missing or exceeds its bounds",
                ));
            }
            if !config.roots.contains_key(cwd_root_id) {
                return Err(Failure::new(
                    FailureCode::UnsafeRoot,
                    "process source names an unknown configured root",
                )
                .for_request(&request.request_id));
            }
            if let Some(environment_profile) = environment_profile
                && (!valid_identifier(environment_profile)
                    || !config
                        .environment_profiles
                        .contains_key(environment_profile))
            {
                return Err(invalid(
                    "process source names an unknown environment profile",
                ));
            }
        }
        Source::Artifact { artifact }
            if artifact.id.len() != 32
                || !artifact.id.bytes().all(is_lower_hex)
                || artifact.source_sha256.len() != 64
                || !artifact.source_sha256.bytes().all(is_lower_hex) =>
        {
            return Err(invalid("artifact reference metadata is invalid"));
        }
        _ => {}
    }
    validate_retention_shape(&request.retention)
        .map_err(|failure| failure.for_request(&request.request_id))
}

pub(crate) fn validate_process(
    executable: &ByteString,
    argv: &[ByteString],
    timeout_ms: Option<u64>,
) -> Result<(), Failure> {
    let argument_bytes = argv.iter().try_fold(executable.0.len(), |total, argument| {
        total.checked_add(argument.0.len())
    });
    if argv.len() > MAX_PROCESS_ARGUMENTS
        || argument_bytes.is_none_or(|total| total > MAX_PROCESS_ARGUMENT_BYTES)
    {
        return Err(Failure::new(
            FailureCode::ResourceExhausted,
            "process executable and argv exceed the configured limits",
        ));
    }
    if executable.0.is_empty()
        || executable.0.len() > MAX_PROCESS_EXECUTABLE_BYTES
        || executable.0.contains(&0)
        || argv.iter().any(|argument| argument.0.contains(&0))
    {
        return Err(Failure::new(
            FailureCode::InvalidRequest,
            "process executable or argv is invalid",
        ));
    }
    let timeout_ms = timeout_ms.unwrap_or(DEFAULT_PROCESS_TIMEOUT_MS);
    if !(MIN_PROCESS_TIMEOUT_MS..=MAX_PROCESS_TIMEOUT_MS).contains(&timeout_ms) {
        return Err(Failure::new(
            FailureCode::InvalidRequest,
            "process timeout must be from 100 ms through 300 seconds",
        ));
    }
    Ok(())
}

pub(crate) fn resolve_expiration(
    retention: &Retention,
    now: u64,
    default_ttl: u64,
) -> Result<u64, Failure> {
    validate_retention_shape(retention)?;
    match (retention.expires_at, retention.ttl_seconds) {
        (Some(expires_at), None) if expires_at > now => Ok(expires_at),
        (Some(_), None) => Err(Failure::new(
            FailureCode::InvalidRequest,
            "artifact expiration must be in the future",
        )),
        (None, Some(ttl)) => now.checked_add(ttl).ok_or_else(|| {
            Failure::new(
                FailureCode::InvalidRequest,
                "artifact expiration overflows the supported clock",
            )
        }),
        (None, None) => now.checked_add(default_ttl).ok_or_else(|| {
            Failure::new(
                FailureCode::InvalidRequest,
                "default artifact expiration overflows the supported clock",
            )
        }),
        (Some(_), Some(_)) => Err(Failure::new(
            FailureCode::InvariantBreach,
            "validated retention shape became inconsistent",
        )),
    }
}

fn validate_retention_shape(retention: &Retention) -> Result<(), Failure> {
    match (retention.expires_at, retention.ttl_seconds) {
        (Some(_), Some(_)) => Err(Failure::new(
            FailureCode::InvalidRequest,
            "retention must specify expires_at or ttl_seconds, not both",
        )),
        (None, Some(0)) => Err(Failure::new(
            FailureCode::InvalidRequest,
            "artifact retention must be positive",
        )),
        _ => Ok(()),
    }
}

fn correlate(request: &Request, failure: Failure) -> Failure {
    if valid_correlation_id(&request.request_id) {
        failure.for_request(&request.request_id)
    } else {
        failure
    }
}

fn valid_correlation_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTIFIER_BYTES
}

pub(crate) fn valid_identifier(value: &str) -> bool {
    valid_correlation_id(value)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}
