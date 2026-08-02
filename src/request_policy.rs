use crate::{
    contract::{MAX_PATH_BYTES, is_lower_hex, valid_correlation_id, valid_identifier},
    projection::ProjectionSpec,
    types::{
        ArtifactRef, ByteString, CONTRACT_VERSION, EngineConfig, Failure, FailureCode, Request,
        Retention, Source,
    },
};

pub const MAX_PROCESS_ARGUMENTS: usize = 4_096;
pub const MAX_PROCESS_ARGUMENT_BYTES: usize = 1024 * 1024;
pub const MAX_PROCESS_EXECUTABLE_BYTES: usize = 4_096;
pub const MAX_SOURCE_BYTES: usize = 10 * 1024 * 1024;
pub const MIN_PROCESS_TIMEOUT_MS: u64 = 100;
pub const MAX_PROCESS_TIMEOUT_MS: u64 = 300_000;
pub(crate) const DEFAULT_PROCESS_TIMEOUT_MS: u64 = 30_000;

#[derive(Clone, Debug)]
pub(crate) struct ValidatedRequest {
    pub request_id: String,
    pub source: ValidatedSource,
    pub projection: ProjectionSpec,
    pub retention: ValidatedRetention,
}

#[derive(Clone, Debug)]
pub(crate) enum ValidatedSource {
    Local(LocalSource),
    Artifact(ArtifactRef),
}

#[derive(Clone, Debug)]
pub(crate) enum LocalSource {
    Inline {
        bytes: ByteString,
    },
    File {
        root_id: String,
        relative_path: ByteString,
        reject_binary: bool,
    },
    Process(ProcessSource),
}

#[derive(Clone, Debug)]
pub(crate) struct ProcessSource {
    pub executable: ByteString,
    pub argv: Vec<ByteString>,
    pub cwd_root_id: String,
    pub cwd_relative_path: ByteString,
    pub timeout_ms: u64,
    pub environment_profile: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ValidatedRetention {
    Absolute(u64),
    Ttl(u64),
    Default,
}

impl ValidatedRetention {
    pub(crate) fn resolve(self, now: u64, default_ttl: u64) -> Result<u64, Failure> {
        match self {
            Self::Absolute(expires_at) if expires_at > now => Ok(expires_at),
            Self::Absolute(_) => Err(Failure::new(
                FailureCode::InvalidRequest,
                "artifact expiration must be in the future",
            )),
            Self::Ttl(ttl) => now.checked_add(ttl).ok_or_else(|| {
                Failure::new(
                    FailureCode::InvalidRequest,
                    "artifact expiration overflows the supported clock",
                )
            }),
            Self::Default => now.checked_add(default_ttl).ok_or_else(|| {
                Failure::new(
                    FailureCode::InvalidRequest,
                    "default artifact expiration overflows the supported clock",
                )
            }),
        }
    }
}

pub(crate) fn prepare(
    request: Request,
    config: &EngineConfig,
) -> Result<ValidatedRequest, Failure> {
    if request.contract_version != CONTRACT_VERSION {
        return Err(correlate(
            &request,
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
    let source = match request.source {
        Source::Inline { bytes, media_type } => {
            if bytes.0.len() > MAX_SOURCE_BYTES {
                return Err(Failure::new(
                    FailureCode::InputTooLarge,
                    "inline source exceeds the 10 MiB limit",
                )
                .for_request(&request.request_id));
            }
            // Contract v2 accepts source media type as advisory metadata only. Projection
            // remains deterministic from bytes and always emits `text/plain`.
            drop(media_type);
            ValidatedSource::Local(LocalSource::Inline { bytes })
        }
        Source::File {
            root_id,
            relative_path,
            binary_policy,
        } => {
            if !valid_identifier(&root_id)
                || relative_path.0.len() > MAX_PATH_BYTES
                || relative_path.0.contains(&0)
            {
                return Err(invalid(
                    "file source metadata is missing or exceeds its bounds",
                ));
            }
            if !config.roots.contains_key(&root_id) {
                return Err(Failure::new(
                    FailureCode::UnsafeRoot,
                    "file source names an unknown root",
                )
                .for_request(&request.request_id));
            }
            ValidatedSource::Local(LocalSource::File {
                root_id,
                relative_path,
                reject_binary: binary_policy == crate::types::BinaryPolicy::Reject,
            })
        }
        Source::Process {
            executable,
            argv,
            cwd_root_id,
            cwd_relative_path,
            timeout_ms,
            environment_profile,
        } => {
            let timeout_ms = validate_process(&executable, &argv, timeout_ms)
                .map_err(|failure| failure.for_request(&request.request_id))?;
            if !valid_identifier(&cwd_root_id)
                || cwd_relative_path.0.len() > MAX_PATH_BYTES
                || cwd_relative_path.0.contains(&0)
            {
                return Err(invalid(
                    "process source metadata is missing or exceeds its bounds",
                ));
            }
            if !config.roots.contains_key(&cwd_root_id) {
                return Err(Failure::new(
                    FailureCode::UnsafeRoot,
                    "process source names an unknown configured root",
                )
                .for_request(&request.request_id));
            }
            if let Some(environment_profile) = environment_profile.as_deref()
                && (!valid_identifier(environment_profile)
                    || !config
                        .environment_profiles
                        .contains_key(environment_profile))
            {
                return Err(invalid(
                    "process source names an unknown environment profile",
                ));
            }
            ValidatedSource::Local(LocalSource::Process(ProcessSource {
                executable,
                argv,
                cwd_root_id,
                cwd_relative_path,
                timeout_ms,
                environment_profile,
            }))
        }
        Source::Artifact { artifact } => {
            if artifact.id.len() != 32
                || !artifact.id.bytes().all(is_lower_hex)
                || artifact.source_sha256.len() != 64
                || !artifact.source_sha256.bytes().all(is_lower_hex)
            {
                return Err(invalid("artifact reference metadata is invalid"));
            }
            ValidatedSource::Artifact(artifact)
        }
    };
    let retention = validate_retention_shape(&request.retention)
        .map_err(|failure| failure.for_request(&request.request_id))?;
    let projection = ProjectionSpec::new(&request.preservation_profile, &request.budget)
        .map_err(|failure| failure.for_request(&request.request_id))?;
    Ok(ValidatedRequest {
        request_id: request.request_id,
        source,
        projection,
        retention,
    })
}

#[cfg(test)]
pub(crate) fn validate(request: &Request, config: &EngineConfig) -> Result<(), Failure> {
    prepare(request.clone(), config).map(|_| ())
}

pub(crate) fn validate_process(
    executable: &ByteString,
    argv: &[ByteString],
    timeout_ms: Option<u64>,
) -> Result<u64, Failure> {
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
    Ok(timeout_ms)
}

#[cfg(test)]
pub(crate) fn resolve_expiration(
    retention: &Retention,
    now: u64,
    default_ttl: u64,
) -> Result<u64, Failure> {
    validate_retention_shape(retention)?.resolve(now, default_ttl)
}

fn validate_retention_shape(retention: &Retention) -> Result<ValidatedRetention, Failure> {
    match (retention.expires_at, retention.ttl_seconds) {
        (Some(_), Some(_)) => Err(Failure::new(
            FailureCode::InvalidRequest,
            "retention must specify expires_at or ttl_seconds, not both",
        )),
        (None, Some(0)) => Err(Failure::new(
            FailureCode::InvalidRequest,
            "artifact retention must be positive",
        )),
        (Some(expires_at), None) => Ok(ValidatedRetention::Absolute(expires_at)),
        (None, Some(ttl)) => Ok(ValidatedRetention::Ttl(ttl)),
        (None, None) => Ok(ValidatedRetention::Default),
    }
}

fn correlate(request: &Request, failure: Failure) -> Failure {
    if valid_correlation_id(&request.request_id) {
        failure.for_request(&request.request_id)
    } else {
        failure
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::MAX_IDENTIFIER_BYTES;
    use crate::types::{ARTIFACT_SCHEMA_VERSION, ArtifactRef, BinaryPolicy, Budget, CountUnit};
    use std::path::PathBuf;

    fn request(source: Source) -> Request {
        Request {
            contract_version: CONTRACT_VERSION.to_owned(),
            request_id: "request".to_owned(),
            source,
            budget: Budget {
                unit: CountUnit::Bytes,
                total_visible_limit: 32,
                reserved_envelope: 0,
                token_profile: None,
            },
            preservation_profile: "plain-text/v1".to_owned(),
            retention: Retention::default(),
        }
    }

    fn artifact() -> ArtifactRef {
        ArtifactRef {
            schema_version: ARTIFACT_SCHEMA_VERSION.to_owned(),
            id: "a".repeat(32),
            source_sha256: "b".repeat(64),
            source_bytes: 3,
            created_at: 1,
            expires_at: 2,
        }
    }

    #[test]
    fn validation_rejects_each_independent_bounded_field() {
        let mut config = EngineConfig::local(PathBuf::from("store.sqlite"));
        config
            .roots
            .insert("workspace".to_owned(), PathBuf::from("workspace"));
        config
            .environment_profiles
            .insert("safe".to_owned(), Default::default());

        let mut unsupported_contract = request(Source::Inline {
            bytes: ByteString::default(),
            media_type: None,
        });
        unsupported_contract.contract_version = "unsupported".to_owned();
        let failure = validate(&unsupported_contract, &config).expect_err("contract");
        assert_eq!(failure.code, FailureCode::SchemaUnsupported);
        assert_eq!(failure.request_id.as_deref(), Some("request"));
        unsupported_contract.request_id.clear();
        assert!(
            validate(&unsupported_contract, &config)
                .expect_err("uncorrelated contract")
                .request_id
                .is_none()
        );

        let mut long_request_id = request(Source::Inline {
            bytes: ByteString::default(),
            media_type: None,
        });
        long_request_id.request_id = "r".repeat(MAX_IDENTIFIER_BYTES + 1);
        assert_eq!(
            validate(&long_request_id, &config)
                .expect_err("request ID")
                .code,
            FailureCode::InvalidRequest
        );

        let oversized_inline = request(Source::Inline {
            bytes: ByteString(vec![0; MAX_SOURCE_BYTES + 1]),
            media_type: None,
        });
        assert_eq!(
            validate(&oversized_inline, &config)
                .expect_err("inline limit")
                .code,
            FailureCode::InputTooLarge
        );

        let invalid_file_root = request(Source::File {
            root_id: String::new(),
            relative_path: ByteString::default(),
            binary_policy: BinaryPolicy::Accept,
        });
        assert_eq!(
            validate(&invalid_file_root, &config)
                .expect_err("file root")
                .code,
            FailureCode::InvalidRequest
        );
        let oversized_file_path = request(Source::File {
            root_id: "workspace".to_owned(),
            relative_path: ByteString(vec![b'p'; MAX_PATH_BYTES + 1]),
            binary_policy: BinaryPolicy::Accept,
        });
        assert_eq!(
            validate(&oversized_file_path, &config)
                .expect_err("file path limit")
                .code,
            FailureCode::InvalidRequest
        );
        let nul_file_path = request(Source::File {
            root_id: "workspace".to_owned(),
            relative_path: ByteString(b"nul\0path".to_vec()),
            binary_policy: BinaryPolicy::Accept,
        });
        assert_eq!(
            validate(&nul_file_path, &config)
                .expect_err("file path NUL")
                .code,
            FailureCode::InvalidRequest
        );
        let unknown_file_root = request(Source::File {
            root_id: "unknown".to_owned(),
            relative_path: ByteString::default(),
            binary_policy: BinaryPolicy::Accept,
        });
        assert_eq!(
            validate(&unknown_file_root, &config)
                .expect_err("unknown file root")
                .code,
            FailureCode::UnsafeRoot
        );

        assert_eq!(
            validate_process(
                &ByteString::from_utf8("/bin/true"),
                &vec![ByteString::default(); MAX_PROCESS_ARGUMENTS + 1],
                None,
            )
            .expect_err("argument count")
            .code,
            FailureCode::ResourceExhausted
        );
        assert_eq!(
            validate_process(
                &ByteString::from_utf8("/bin/true"),
                &[ByteString(vec![b'a'; MAX_PROCESS_ARGUMENT_BYTES])],
                None,
            )
            .expect_err("argument bytes")
            .code,
            FailureCode::ResourceExhausted
        );
        assert_eq!(
            validate_process(&ByteString::default(), &[], None)
                .expect_err("empty executable")
                .code,
            FailureCode::InvalidRequest
        );
        assert_eq!(
            validate_process(
                &ByteString(vec![b'x'; MAX_PROCESS_EXECUTABLE_BYTES + 1]),
                &[],
                None,
            )
            .expect_err("executable length")
            .code,
            FailureCode::InvalidRequest
        );
        assert_eq!(
            validate_process(&ByteString(b"/bin/tr\0ue".to_vec()), &[], None)
                .expect_err("executable NUL")
                .code,
            FailureCode::InvalidRequest
        );
        assert_eq!(
            validate_process(
                &ByteString::from_utf8("/bin/true"),
                &[ByteString(b"nul\0argument".to_vec())],
                None,
            )
            .expect_err("argument NUL")
            .code,
            FailureCode::InvalidRequest
        );
        assert_eq!(
            validate_process(
                &ByteString::from_utf8("/bin/true"),
                &[],
                Some(MAX_PROCESS_TIMEOUT_MS + 1),
            )
            .expect_err("timeout")
            .code,
            FailureCode::InvalidRequest
        );

        let invalid_process_root = request(Source::Process {
            executable: ByteString::from_utf8("/bin/true"),
            argv: Vec::new(),
            cwd_root_id: String::new(),
            cwd_relative_path: ByteString::default(),
            timeout_ms: None,
            environment_profile: None,
        });
        assert_eq!(
            validate(&invalid_process_root, &config)
                .expect_err("process root")
                .code,
            FailureCode::InvalidRequest
        );
        let oversized_process_path = request(Source::Process {
            executable: ByteString::from_utf8("/bin/true"),
            argv: Vec::new(),
            cwd_root_id: "workspace".to_owned(),
            cwd_relative_path: ByteString(vec![b'p'; MAX_PATH_BYTES + 1]),
            timeout_ms: None,
            environment_profile: None,
        });
        assert_eq!(
            validate(&oversized_process_path, &config)
                .expect_err("process path limit")
                .code,
            FailureCode::InvalidRequest
        );
        let nul_process_path = request(Source::Process {
            executable: ByteString::from_utf8("/bin/true"),
            argv: Vec::new(),
            cwd_root_id: "workspace".to_owned(),
            cwd_relative_path: ByteString(b"nul\0path".to_vec()),
            timeout_ms: None,
            environment_profile: None,
        });
        assert_eq!(
            validate(&nul_process_path, &config)
                .expect_err("process path NUL")
                .code,
            FailureCode::InvalidRequest
        );
        let invalid_environment = request(Source::Process {
            executable: ByteString::from_utf8("/bin/true"),
            argv: Vec::new(),
            cwd_root_id: "workspace".to_owned(),
            cwd_relative_path: ByteString::default(),
            timeout_ms: None,
            environment_profile: Some("invalid!".to_owned()),
        });
        assert_eq!(
            validate(&invalid_environment, &config)
                .expect_err("invalid environment")
                .code,
            FailureCode::InvalidRequest
        );

        let mut invalid_artifact = artifact();
        invalid_artifact.id = "a".repeat(31);
        assert_eq!(
            validate(
                &request(Source::Artifact {
                    artifact: invalid_artifact
                }),
                &config,
            )
            .expect_err("artifact ID length")
            .code,
            FailureCode::InvalidRequest
        );
        let mut invalid_artifact = artifact();
        invalid_artifact.id = "A".repeat(32);
        assert_eq!(
            validate(
                &request(Source::Artifact {
                    artifact: invalid_artifact
                }),
                &config,
            )
            .expect_err("artifact ID alphabet")
            .code,
            FailureCode::InvalidRequest
        );
        let mut invalid_artifact = artifact();
        invalid_artifact.source_sha256 = "b".repeat(63);
        assert_eq!(
            validate(
                &request(Source::Artifact {
                    artifact: invalid_artifact
                }),
                &config,
            )
            .expect_err("source digest length")
            .code,
            FailureCode::InvalidRequest
        );
        let mut invalid_artifact = artifact();
        invalid_artifact.source_sha256 = "B".repeat(64);
        assert_eq!(
            validate(
                &request(Source::Artifact {
                    artifact: invalid_artifact
                }),
                &config,
            )
            .expect_err("source digest alphabet")
            .code,
            FailureCode::InvalidRequest
        );
    }

    #[test]
    fn inline_media_type_is_advisory_in_contract_v2() {
        let config = EngineConfig::local(PathBuf::from("store.sqlite"));
        let prepared = prepare(
            request(Source::Inline {
                bytes: ByteString::from_utf8("same bytes"),
                media_type: Some("application/octet-stream".to_owned()),
            }),
            &config,
        )
        .expect("advisory media type");

        assert!(matches!(
            &prepared.source,
            ValidatedSource::Local(LocalSource::Inline { .. })
        ));
        if let ValidatedSource::Local(LocalSource::Inline { bytes }) = prepared.source {
            assert_eq!(bytes, ByteString::from_utf8("same bytes"));
        }
    }
}
