use super::*;

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
fn acquisition_semantics_cover_runtime_states_and_reject_contradictions() {
    let inline = AcquisitionReceipt {
        variant: SourceVariant::Inline,
        complete: true,
        partial: false,
        truncated: false,
        root_id: None,
        relative_path: None,
        process: None,
    };
    inline.validate(3).expect("complete inline");
    AcquisitionReceipt {
        complete: false,
        ..inline.clone()
    }
    .validate(0)
    .expect("clean inline failure");

    let file = AcquisitionReceipt {
        variant: SourceVariant::File,
        complete: true,
        partial: false,
        truncated: false,
        root_id: Some("workspace".to_owned()),
        relative_path: Some("<4 path bytes>".to_owned()),
        process: None,
    };
    file.validate(3).expect("complete file");
    AcquisitionReceipt {
        complete: false,
        ..file
    }
    .validate(0)
    .expect("clean file failure");

    let process = AcquisitionReceipt {
        variant: SourceVariant::Process,
        complete: true,
        partial: false,
        truncated: false,
        root_id: Some("workspace".to_owned()),
        relative_path: None,
        process: Some(ProcessReceipt {
            events: vec![StreamEvent {
                order: 0,
                stream: ProcessStream::Stdout,
                span: ByteSpan { start: 0, end: 3 },
            }],
            exit_code: Some(0),
            signal: None,
            timed_out: false,
            working_directory: "workspace:<0 path bytes>".to_owned(),
        }),
    };
    process.validate(3).expect("complete process");
    AcquisitionReceipt {
        complete: false,
        partial: true,
        truncated: false,
        process: Some(ProcessReceipt {
            exit_code: None,
            signal: Some(15),
            timed_out: true,
            ..process.process.clone().expect("process")
        }),
        ..process.clone()
    }
    .validate(3)
    .expect("timed-out partial process");
    AcquisitionReceipt {
        complete: false,
        partial: true,
        truncated: true,
        process: Some(ProcessReceipt {
            exit_code: None,
            signal: Some(9),
            ..process.process.clone().expect("process")
        }),
        ..process.clone()
    }
    .validate(3)
    .expect("truncated partial process");
    AcquisitionReceipt {
        complete: false,
        partial: true,
        process: Some(ProcessReceipt {
            exit_code: Some(1),
            signal: None,
            ..process.process.clone().expect("process")
        }),
        ..process.clone()
    }
    .validate(3)
    .expect("partial process read failure");
    AcquisitionReceipt {
        complete: false,
        process: Some(ProcessReceipt {
            events: Vec::new(),
            exit_code: None,
            signal: None,
            timed_out: false,
            working_directory: "workspace:<0 path bytes>".to_owned(),
        }),
        ..process.clone()
    }
    .validate(0)
    .expect("clean process failure");

    AcquisitionReceipt {
        variant: SourceVariant::Artifact,
        ..inline.clone()
    }
    .validate(3)
    .expect("artifact replay");

    let mut contradictions = Vec::new();
    contradictions.push(AcquisitionReceipt {
        partial: true,
        ..inline.clone()
    });
    contradictions.push(AcquisitionReceipt {
        truncated: true,
        ..inline
    });
    contradictions.push(AcquisitionReceipt {
        process: None,
        ..process.clone()
    });
    contradictions.push(AcquisitionReceipt {
        root_id: None,
        ..process.clone()
    });
    contradictions.push(AcquisitionReceipt {
        process: Some(ProcessReceipt {
            events: vec![StreamEvent {
                order: 1,
                stream: ProcessStream::Stdout,
                span: ByteSpan { start: 0, end: 3 },
            }],
            ..process.process.clone().expect("process")
        }),
        ..process.clone()
    });
    contradictions.push(AcquisitionReceipt {
        process: Some(ProcessReceipt {
            events: vec![
                StreamEvent {
                    order: 0,
                    stream: ProcessStream::Stdout,
                    span: ByteSpan { start: 0, end: 2 },
                },
                StreamEvent {
                    order: 1,
                    stream: ProcessStream::Stderr,
                    span: ByteSpan { start: 1, end: 3 },
                },
            ],
            ..process.process.clone().expect("process")
        }),
        ..process.clone()
    });
    contradictions.push(AcquisitionReceipt {
        process: Some(ProcessReceipt {
            events: vec![StreamEvent {
                order: 0,
                stream: ProcessStream::Stdout,
                span: ByteSpan { start: 0, end: 4 },
            }],
            ..process.process.expect("process")
        }),
        ..process
    });
    for receipt in contradictions {
        assert!(receipt.validate(3).is_err());
    }
}

#[test]
fn validated_acquisition_preserves_wire_path_spelling() {
    let file = AcquisitionReceipt {
        variant: SourceVariant::File,
        complete: true,
        partial: false,
        truncated: false,
        root_id: Some("workspace".to_owned()),
        relative_path: Some("<0004 path bytes>".to_owned()),
        process: None,
    };
    assert_eq!(
        ValidatedAcquisition::from_wire(file.clone(), 3)
            .expect("validated file")
            .to_receipt(),
        file
    );

    let process = AcquisitionReceipt {
        variant: SourceVariant::Process,
        complete: true,
        partial: false,
        truncated: false,
        root_id: Some("workspace".to_owned()),
        relative_path: None,
        process: Some(ProcessReceipt {
            events: vec![StreamEvent {
                order: 0,
                stream: ProcessStream::Stdout,
                span: ByteSpan { start: 0, end: 3 },
            }],
            exit_code: Some(0),
            signal: None,
            timed_out: false,
            working_directory: "workspace:<000 path bytes>".to_owned(),
        }),
    };
    assert_eq!(
        ValidatedAcquisition::from_wire(process.clone(), 3)
            .expect("validated process")
            .to_receipt(),
        process
    );
}

#[test]
fn acquisition_semantics_reject_each_invalid_metadata_shape() {
    let inline = AcquisitionReceipt {
        variant: SourceVariant::Inline,
        complete: true,
        partial: false,
        truncated: false,
        root_id: None,
        relative_path: None,
        process: None,
    };
    let file = AcquisitionReceipt {
        variant: SourceVariant::File,
        complete: true,
        partial: false,
        truncated: false,
        root_id: Some("workspace".to_owned()),
        relative_path: Some("<4 path bytes>".to_owned()),
        process: None,
    };
    let process = AcquisitionReceipt {
        variant: SourceVariant::Process,
        complete: true,
        partial: false,
        truncated: false,
        root_id: Some("workspace".to_owned()),
        relative_path: None,
        process: Some(ProcessReceipt {
            events: vec![StreamEvent {
                order: 0,
                stream: ProcessStream::Stdout,
                span: ByteSpan { start: 0, end: 3 },
            }],
            exit_code: Some(0),
            signal: None,
            timed_out: false,
            working_directory: "workspace:<0 path bytes>".to_owned(),
        }),
    };

    let invalid_receipts = [
        (
            AcquisitionReceipt {
                complete: false,
                partial: true,
                ..inline.clone()
            },
            3,
        ),
        (
            AcquisitionReceipt {
                complete: false,
                ..inline.clone()
            },
            1,
        ),
        (
            AcquisitionReceipt {
                root_id: Some("workspace".to_owned()),
                ..inline.clone()
            },
            3,
        ),
        (
            AcquisitionReceipt {
                relative_path: Some("<0 path bytes>".to_owned()),
                ..inline.clone()
            },
            3,
        ),
        (
            AcquisitionReceipt {
                process: process.process.clone(),
                ..inline
            },
            3,
        ),
        (
            AcquisitionReceipt {
                root_id: None,
                ..file.clone()
            },
            3,
        ),
        (
            AcquisitionReceipt {
                root_id: Some(String::new()),
                ..file.clone()
            },
            3,
        ),
        (
            AcquisitionReceipt {
                relative_path: None,
                ..file.clone()
            },
            3,
        ),
        (
            AcquisitionReceipt {
                relative_path: Some("4 path bytes>".to_owned()),
                ..file.clone()
            },
            3,
        ),
        (
            AcquisitionReceipt {
                relative_path: Some("<4 path bytes".to_owned()),
                ..file.clone()
            },
            3,
        ),
        (
            AcquisitionReceipt {
                relative_path: Some("< path bytes>".to_owned()),
                ..file.clone()
            },
            3,
        ),
        (
            AcquisitionReceipt {
                relative_path: Some("<four path bytes>".to_owned()),
                ..file.clone()
            },
            3,
        ),
        (
            AcquisitionReceipt {
                relative_path: Some(format!(
                    "<{} path bytes>",
                    crate::contract::MAX_PATH_BYTES as u64 + 1
                )),
                ..file.clone()
            },
            3,
        ),
        (
            AcquisitionReceipt {
                process: process.process.clone(),
                ..file
            },
            3,
        ),
        (
            AcquisitionReceipt {
                root_id: Some(String::new()),
                ..process.clone()
            },
            3,
        ),
        (
            AcquisitionReceipt {
                relative_path: Some("<0 path bytes>".to_owned()),
                ..process.clone()
            },
            3,
        ),
        (
            AcquisitionReceipt {
                process: Some(ProcessReceipt {
                    working_directory: "other:<0 path bytes>".to_owned(),
                    ..process.process.clone().expect("process")
                }),
                ..process.clone()
            },
            3,
        ),
        (
            AcquisitionReceipt {
                process: Some(ProcessReceipt {
                    working_directory: "workspace:invalid".to_owned(),
                    ..process.process.clone().expect("process")
                }),
                ..process.clone()
            },
            3,
        ),
        (
            AcquisitionReceipt {
                process: Some(ProcessReceipt {
                    events: vec![StreamEvent {
                        order: 0,
                        stream: ProcessStream::Stdout,
                        span: ByteSpan { start: 0, end: 0 },
                    }],
                    ..process.process.clone().expect("process")
                }),
                ..process.clone()
            },
            0,
        ),
        (
            AcquisitionReceipt {
                process: Some(ProcessReceipt {
                    events: Vec::new(),
                    ..process.process.clone().expect("process")
                }),
                ..process.clone()
            },
            3,
        ),
        (
            AcquisitionReceipt {
                complete: false,
                process: Some(ProcessReceipt {
                    events: Vec::new(),
                    exit_code: Some(1),
                    signal: None,
                    timed_out: false,
                    ..process.process.clone().expect("process")
                }),
                ..process.clone()
            },
            0,
        ),
        (
            AcquisitionReceipt {
                complete: false,
                process: Some(ProcessReceipt {
                    events: Vec::new(),
                    exit_code: None,
                    signal: Some(15),
                    timed_out: false,
                    ..process.process.clone().expect("process")
                }),
                ..process.clone()
            },
            0,
        ),
        (
            AcquisitionReceipt {
                complete: false,
                process: Some(ProcessReceipt {
                    events: Vec::new(),
                    exit_code: None,
                    signal: None,
                    timed_out: true,
                    ..process.process.clone().expect("process")
                }),
                ..process.clone()
            },
            0,
        ),
        (
            AcquisitionReceipt {
                process: Some(ProcessReceipt {
                    exit_code: None,
                    signal: None,
                    ..process.process.clone().expect("process")
                }),
                ..process.clone()
            },
            3,
        ),
        (
            AcquisitionReceipt {
                process: Some(ProcessReceipt {
                    exit_code: Some(1),
                    signal: Some(15),
                    ..process.process.clone().expect("process")
                }),
                ..process.clone()
            },
            3,
        ),
        (
            AcquisitionReceipt {
                process: Some(ProcessReceipt {
                    timed_out: true,
                    ..process.process.clone().expect("process")
                }),
                ..process.clone()
            },
            3,
        ),
        (
            AcquisitionReceipt {
                process: Some(ProcessReceipt {
                    exit_code: None,
                    signal: Some(15),
                    ..process.process.expect("process")
                }),
                ..process
            },
            3,
        ),
    ];

    for (receipt, source_bytes) in invalid_receipts {
        assert!(
            receipt.validate(source_bytes).is_err(),
            "invalid receipt was accepted: {receipt:?}"
        );
    }
}

#[test]
fn jsonl_request_decoding_is_bounded_strict_and_versioned() {
    let request = Request {
        contract_version: CONTRACT_VERSION.to_owned(),
        request_id: "round-trip".to_owned(),
        source: Source::Inline {
            bytes: ByteString::from(&[0, 1, 255][..]),
            media_type: None,
        },
        budget: Budget {
            unit: CountUnit::Bytes,
            total_visible_limit: 32,
            reserved_envelope: 4,
            token_profile: None,
        },
        preservation_profile: "plain-text/v1".to_owned(),
        retention: Retention::default(),
        focus: None,
    };
    let mut line = serde_json::to_vec(&request).expect("serialize");
    line.push(b'\n');
    assert_eq!(Request::from_jsonl(&line).expect("decode"), request);

    let mut v1 = serde_json::to_value(&request).expect("value");
    v1["contract_version"] = serde_json::json!("distill.context/v1");
    v1["metadata"] = serde_json::json!({});
    let failure = Request::from_jsonl(&serde_json::to_vec(&v1).expect("serialize v1"))
        .expect_err("v1 schema");
    assert_eq!(failure.code, FailureCode::SchemaUnsupported);
    assert_eq!(failure.request_id.as_deref(), Some("round-trip"));

    let mut removed_metadata = serde_json::to_value(&request).expect("value");
    removed_metadata["metadata"] = serde_json::json!({});
    let failure =
        Request::from_jsonl(&serde_json::to_vec(&removed_metadata).expect("serialize metadata"))
            .expect_err("removed metadata");
    assert_eq!(failure.code, FailureCode::InvalidRequest);
    assert_eq!(failure.request_id.as_deref(), Some("round-trip"));

    let mut invalid_typed_field = serde_json::to_value(&request).expect("value");
    invalid_typed_field["budget"]["total_visible_limit"] = serde_json::json!("invalid");
    let failure = Request::from_jsonl(
        &serde_json::to_vec(&invalid_typed_field).expect("serialize typed failure"),
    )
    .expect_err("typed failure");
    assert_eq!(failure.code, FailureCode::InvalidRequest);
    assert_eq!(failure.request_id.as_deref(), Some("round-trip"));

    let mut missing = serde_json::to_value(&request).expect("value");
    missing.as_object_mut().expect("object").remove("budget");
    let missing_failure =
        Request::from_jsonl(&serde_json::to_vec(&missing).expect("serialize missing field"))
            .expect_err("missing field");
    let mut unknown = serde_json::to_value(&request).expect("value");
    unknown["unknown"] = serde_json::json!(true);
    let unknown_failure =
        Request::from_jsonl(&serde_json::to_vec(&unknown).expect("serialize unknown field"))
            .expect_err("unknown field");
    assert_eq!(missing_failure.code, FailureCode::InvalidRequest);
    assert_eq!(missing_failure.request_id.as_deref(), Some("round-trip"));
    assert_eq!(unknown_failure.code, missing_failure.code);
    assert_eq!(unknown_failure.safe_message, missing_failure.safe_message);
    assert_eq!(unknown_failure.request_id, missing_failure.request_id);

    let malformed =
        Request::from_jsonl(br#"{"request_id":"unfinished"#).expect_err("malformed request");
    assert_eq!(malformed.code, FailureCode::InvalidRequest);
    assert!(malformed.request_id.is_none());
    assert_eq!(
        {
            let oversized =
                Request::from_jsonl(&vec![b'x'; 16 * 1024 * 1024 + 1]).expect_err("oversized");
            assert!(oversized.request_id.is_none());
            assert_eq!(
                oversized.safe_message,
                "JSONL request exceeds the 16 MiB protocol limit"
            );
            oversized.code
        },
        FailureCode::InputTooLarge,
    );

    let mut unsupported = serde_json::to_value(&request).expect("value");
    unsupported["source"]["kind"] = serde_json::Value::String("remote_url".to_owned());
    let failure = Request::from_jsonl(&serde_json::to_vec(&unsupported).expect("unknown source"))
        .expect_err("unsupported source");
    assert_eq!(failure.code, FailureCode::SourceUnsupported);
    assert_eq!(failure.request_id.as_deref(), Some("round-trip"));
}

#[test]
fn jsonl_request_without_source_is_invalid() {
    let failure = Request::from_jsonl(b"{}").expect_err("missing source");

    assert_eq!(failure.code, FailureCode::InvalidRequest);
}

#[test]
fn every_source_variant_and_failure_round_trips() {
    let variants = [
        Source::Inline {
            bytes: ByteString::from(&[0, 255][..]),
            media_type: Some("application/octet-stream".to_owned()),
        },
        Source::File {
            root_id: "root".to_owned(),
            relative_path: ByteString::from_utf8("src/lib.rs"),
            binary_policy: BinaryPolicy::Reject,
        },
        Source::Process {
            executable: ByteString::from_utf8("/usr/bin/printf"),
            argv: vec![ByteString::from_utf8("--literal")],
            cwd_root_id: "root".to_owned(),
            cwd_relative_path: ByteString::from_utf8("workspace"),
            timeout_ms: Some(1_000),
            environment_profile: Some("safe".to_owned()),
        },
        Source::Artifact {
            artifact: artifact(),
            selector: None,
        },
    ];
    for source in variants {
        let encoded = serde_json::to_vec(&source).expect("source JSON");
        assert_eq!(
            serde_json::from_slice::<Source>(&encoded).expect("source decode"),
            source
        );
    }

    let failure = Failure {
        code: FailureCode::ArtifactCorrupt,
        safe_message: "integrity failure".to_owned(),
        request_id: Some("request".to_owned()),
        details: BTreeMap::from([
            ("retryable".to_owned(), ScalarValue::Boolean(false)),
            ("attempt".to_owned(), ScalarValue::Integer(1)),
        ]),
        artifact: Some(artifact()),
        acquisition: None,
    };
    let encoded = serde_json::to_vec(&failure).expect("failure JSON");
    assert_eq!(
        serde_json::from_slice::<Failure>(&encoded).expect("failure decode"),
        failure
    );
}

#[test]
fn noncanonical_base64_is_rejected() {
    assert!(serde_json::from_str::<ByteString>("\"YQ\"").is_err());
}
