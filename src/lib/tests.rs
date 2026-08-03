use super::*;
use tempfile::TempDir;

fn request(bytes: &[u8], limit: u64) -> Request {
    Request {
        contract_version: CONTRACT_VERSION.to_owned(),
        request_id: "request-1".to_owned(),
        source: Source::Inline {
            bytes: ByteString::from(bytes),
            media_type: Some("text/plain".to_owned()),
        },
        budget: Budget {
            unit: CountUnit::Bytes,
            total_visible_limit: limit,
            reserved_envelope: 0,
            token_profile: None,
        },
        preservation_profile: "plain-text/v1".to_owned(),
        retention: Retention::default(),
    }
}

fn fixture() -> (TempDir, Engine) {
    let directory = tempfile::tempdir().expect("temp directory");
    let config = EngineConfig::local(directory.path().join("store/store.sqlite"));
    let engine = Engine::fixture(config, 1_000).expect("fixture engine");
    (directory, engine)
}

#[test]
fn exact_content_round_trips_through_artifact_source() {
    let (_directory, engine) = fixture();
    let first = engine
        .handle(request(b"exact content", 64))
        .expect("first outcome");
    assert_eq!(first.visible.bytes, "exact content");
    assert_eq!(first.receipt.fidelity, Fidelity::Exact);

    let mut second_request = request(b"ignored", 64);
    second_request.request_id = "request-2".to_owned();
    second_request.source = Source::Artifact {
        artifact: first.artifact.clone(),
        selector: None,
    };
    let second = engine.handle(second_request).expect("artifact outcome");
    assert_eq!(second.visible.bytes, "exact content");
    assert_eq!(second.receipt.acquisition.variant, SourceVariant::Artifact);
    assert_eq!(second.artifact, first.artifact);
    let trace = engine.trace(&first.artifact).expect("artifact trace");
    assert_eq!(
        trace
            .receipts
            .iter()
            .map(|receipt| receipt.request_id.as_str())
            .collect::<Vec<_>>(),
        vec!["request-1", "request-2"]
    );
}

#[test]
fn impossible_budget_returns_committed_artifact() {
    let (_directory, engine) = fixture();
    let mut candidate = request(b"ERROR critical\nnoise\n", 2);
    candidate.preservation_profile = "build-log/v1".to_owned();
    let failure = engine.handle(candidate).expect_err("budget failure");
    assert_eq!(failure.code, FailureCode::BudgetUnsatisfiable);
    assert!(failure.artifact.is_some());
}

#[test]
fn validation_failures_are_distinct_and_precede_capture() {
    let (_directory, engine) = fixture();
    let mut schema = request(b"x", 1);
    schema.contract_version = "distill.context/v1".to_owned();
    assert_eq!(
        engine.handle(schema).expect_err("schema").code,
        FailureCode::SchemaUnsupported
    );

    let mut token = request(b"x", 1);
    token.budget.unit = CountUnit::Tokens;
    token.budget.token_profile = Some("unknown@v1".to_owned());
    assert_eq!(
        engine.handle(token).expect_err("token").code,
        FailureCode::TokenProfileUnsupported
    );
}

#[test]
fn request_validation_exercises_every_bounded_source_field() {
    fn rejected(candidate: &Request, expected: FailureCode) {
        let config = EngineConfig::local("/tmp/distill-policy-test/store.sqlite".into());
        assert_eq!(
            request_policy::validate(candidate, &config)
                .expect_err("invalid request")
                .code,
            expected
        );
    }

    let mut candidate = request(b"x", 1);
    candidate.request_id.clear();
    rejected(&candidate, FailureCode::InvalidRequest);
    candidate.request_id = "x".repeat(129);
    rejected(&candidate, FailureCode::InvalidRequest);

    candidate = request(&vec![0; MAX_SOURCE_BYTES + 1], 1);
    rejected(&candidate, FailureCode::InputTooLarge);

    for (root_id, relative_path) in [
        (String::new(), ByteString::default()),
        ("x".repeat(129), ByteString::default()),
        ("workspace".to_owned(), ByteString(vec![b'x'; 4_097])),
    ] {
        candidate = request(b"x", 1);
        candidate.source = Source::File {
            root_id,
            relative_path,
            binary_policy: BinaryPolicy::Accept,
        };
        rejected(&candidate, FailureCode::InvalidRequest);
    }

    let process = |executable: ByteString,
                   argv: Vec<ByteString>,
                   cwd_root_id: String,
                   cwd_relative_path: ByteString| Source::Process {
        executable,
        argv,
        cwd_root_id,
        cwd_relative_path,
        timeout_ms: None,
        environment_profile: None,
    };
    let process_cases = [
        (
            process(
                ByteString::default(),
                Vec::new(),
                "workspace".to_owned(),
                ByteString::default(),
            ),
            FailureCode::InvalidRequest,
        ),
        (
            process(
                ByteString(vec![b'x'; 4_097]),
                Vec::new(),
                "workspace".to_owned(),
                ByteString::default(),
            ),
            FailureCode::InvalidRequest,
        ),
        (
            process(
                ByteString::from_utf8("/bin/echo"),
                vec![ByteString::default(); 4_097],
                "workspace".to_owned(),
                ByteString::default(),
            ),
            FailureCode::ResourceExhausted,
        ),
        (
            process(
                ByteString::from_utf8("/bin/echo"),
                vec![ByteString(vec![b'x'; 1024 * 1024 + 1])],
                "workspace".to_owned(),
                ByteString::default(),
            ),
            FailureCode::ResourceExhausted,
        ),
        (
            process(
                ByteString::from_utf8("/bin/echo"),
                Vec::new(),
                String::new(),
                ByteString::default(),
            ),
            FailureCode::InvalidRequest,
        ),
        (
            process(
                ByteString::from_utf8("/bin/echo"),
                Vec::new(),
                "x".repeat(129),
                ByteString::default(),
            ),
            FailureCode::InvalidRequest,
        ),
        (
            process(
                ByteString::from_utf8("/bin/echo"),
                Vec::new(),
                "workspace".to_owned(),
                ByteString(vec![b'x'; 4_097]),
            ),
            FailureCode::InvalidRequest,
        ),
    ];
    for (source, expected) in process_cases {
        candidate = request(b"x", 1);
        candidate.source = source;
        rejected(&candidate, expected);
    }

    let valid_artifact = ArtifactRef {
        schema_version: ARTIFACT_SCHEMA_VERSION.to_owned(),
        id: "a".repeat(32),
        source_sha256: "b".repeat(64),
        source_bytes: 1,
        created_at: 1,
        expires_at: 2,
    };
    for artifact in [
        ArtifactRef {
            id: "a".repeat(31),
            ..valid_artifact.clone()
        },
        ArtifactRef {
            id: "A".repeat(32),
            ..valid_artifact.clone()
        },
        ArtifactRef {
            source_sha256: "b".repeat(63),
            ..valid_artifact.clone()
        },
        ArtifactRef {
            source_sha256: "G".repeat(64),
            ..valid_artifact
        },
    ] {
        candidate = request(b"x", 1);
        candidate.source = Source::Artifact {
            artifact,
            selector: None,
        };
        rejected(&candidate, FailureCode::InvalidRequest);
    }
}

#[test]
fn process_argument_policy_accepts_exact_limit_and_rejects_one_byte_over() {
    let executable = ByteString::from_utf8("/bin/echo");
    let remaining = MAX_PROCESS_ARGUMENT_BYTES - executable.0.len();
    let width = remaining / MAX_PROCESS_ARGUMENTS;
    let remainder = remaining % MAX_PROCESS_ARGUMENTS;
    let mut argv = (0..MAX_PROCESS_ARGUMENTS)
        .map(|index| ByteString(vec![b'x'; width + usize::from(index < remainder)]))
        .collect::<Vec<_>>();

    request_policy::validate_process(&executable, &argv, Some(MIN_PROCESS_TIMEOUT_MS))
        .expect("exact aggregate limit");
    argv.last_mut().expect("last argument").0.push(b'x');
    assert_eq!(
        request_policy::validate_process(&executable, &argv, Some(MIN_PROCESS_TIMEOUT_MS),)
            .expect_err("one byte over")
            .code,
        FailureCode::ResourceExhausted
    );
}

#[test]
fn over_limit_process_is_rejected_before_spawn_or_store_mutation() {
    let directory = tempfile::tempdir().expect("temp directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let marker = workspace.join("must-not-exist");
    let shell = ["/bin/sh", "/usr/bin/sh"]
        .into_iter()
        .find(|path| std::path::Path::new(path).exists())
        .expect("shell");
    let script = "touch \"$0\"";
    let fixed_bytes = shell.len() + 2 + script.len() + marker.as_os_str().len();
    let padding = "x".repeat(MAX_PROCESS_ARGUMENT_BYTES + 1 - fixed_bytes);
    let mut config = EngineConfig::local(directory.path().join("private/store.sqlite"));
    config.roots.insert("workspace".to_owned(), workspace);
    let engine = Engine::new(config).expect("engine");
    let mut candidate = request(b"unused", 64);
    candidate.request_id = "over-limit-process".to_owned();
    candidate.source = Source::Process {
        executable: ByteString::from_utf8(shell),
        argv: vec![
            ByteString::from_utf8("-c"),
            ByteString::from_utf8(script),
            ByteString::from_utf8(marker.to_string_lossy()),
            ByteString::from_utf8(padding),
        ],
        cwd_root_id: "workspace".to_owned(),
        cwd_relative_path: ByteString::default(),
        timeout_ms: Some(MIN_PROCESS_TIMEOUT_MS),
        environment_profile: None,
    };

    let failure = engine.handle(candidate).expect_err("over-limit process");
    assert_eq!(failure.code, FailureCode::ResourceExhausted);
    assert_eq!(failure.request_id.as_deref(), Some("over-limit-process"));
    assert!(!marker.exists());
    assert_eq!(engine.status().expect("status").store_records, 0);
}

#[test]
fn public_types_round_trip_without_information_loss() {
    let request = request(&[0, 1, 2, 255], 100);
    let json = serde_json::to_string(&request).expect("serialize");
    let decoded: Request = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(decoded, request);

    let outcome = Outcome {
        visible: VisiblePayload {
            bytes: "ok".to_owned(),
            media_type: "text/plain".to_owned(),
        },
        artifact: ArtifactRef {
            schema_version: ARTIFACT_SCHEMA_VERSION.to_owned(),
            id: "0".repeat(32),
            source_sha256: "a".repeat(64),
            source_bytes: 2,
            created_at: 1,
            expires_at: 2,
        },
        receipt: Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION.to_owned(),
            request_id: "id".to_owned(),
            source_sha256: "a".repeat(64),
            artifact: ArtifactRef {
                schema_version: ARTIFACT_SCHEMA_VERSION.to_owned(),
                id: "0".repeat(32),
                source_sha256: "a".repeat(64),
                source_bytes: 2,
                created_at: 1,
                expires_at: 2,
            },
            projection_version: PROJECTION_VERSION.to_owned(),
            policy_version: POLICY_VERSION.to_owned(),
            token_profile: None,
            original_count: 2,
            visible_count: 2,
            count_unit: CountUnit::Bytes,
            fidelity: Fidelity::Exact,
            retained_spans: vec![ByteSpan { start: 0, end: 2 }],
            omitted_spans: Vec::new(),
            preservation: PreservationResult {
                profile: "plain-text/v1".to_owned(),
                mandatory_fact_ids: Vec::new(),
            },
            acquisition: AcquisitionReceipt {
                variant: SourceVariant::Inline,
                complete: true,
                partial: false,
                truncated: false,
                root_id: None,
                relative_path: None,
                process: None,
            },
        },
    };
    let json = serde_json::to_string(&outcome).expect("serialize outcome");
    assert_eq!(
        serde_json::from_str::<Outcome>(&json).expect("deserialize outcome"),
        outcome
    );
}

#[test]
fn receipt_fields_are_stable_across_one_hundred_repetitions() {
    let (_directory, engine) = fixture();
    let mut candidate = request(
        b"head\nnoise noise noise\nERROR E1: critical\nwarning W1: useful\ntail\n",
        48,
    );
    candidate.preservation_profile = "build-log/v1".to_owned();
    let first = engine.handle(candidate.clone()).expect("first");
    for _ in 0..99 {
        let repeated = engine.handle(candidate.clone()).expect("repeat");
        assert_eq!(repeated.visible, first.visible);
        assert_eq!(repeated.receipt.source_sha256, first.receipt.source_sha256);
        assert_eq!(
            repeated.receipt.projection_version,
            first.receipt.projection_version
        );
        assert_eq!(
            repeated.receipt.policy_version,
            first.receipt.policy_version
        );
        assert_eq!(
            repeated.receipt.original_count,
            first.receipt.original_count
        );
        assert_eq!(repeated.receipt.visible_count, first.receipt.visible_count);
        assert_eq!(repeated.receipt.fidelity, first.receipt.fidelity);
        assert_eq!(
            repeated.receipt.retained_spans,
            first.receipt.retained_spans
        );
        assert_eq!(repeated.receipt.omitted_spans, first.receipt.omitted_spans);
        assert_eq!(repeated.receipt.preservation, first.receipt.preservation);
    }
}

#[test]
fn retention_resolution_and_configuration_are_strict() {
    assert_eq!(
        request_policy::resolve_expiration(
            &Retention {
                expires_at: Some(20),
                ttl_seconds: Some(10)
            },
            10,
            100
        )
        .expect_err("ambiguous")
        .code,
        FailureCode::InvalidRequest
    );
    assert_eq!(
        request_policy::resolve_expiration(
            &Retention {
                expires_at: Some(10),
                ttl_seconds: None
            },
            10,
            100
        )
        .expect_err("stale")
        .code,
        FailureCode::InvalidRequest
    );
    assert_eq!(
        request_policy::resolve_expiration(&Retention::default(), 10, 100).expect("default"),
        110
    );
    assert_eq!(
        request_policy::resolve_expiration(
            &Retention {
                expires_at: None,
                ttl_seconds: Some(1)
            },
            u64::MAX,
            100
        )
        .expect_err("ttl overflow")
        .code,
        FailureCode::InvalidRequest
    );
    assert_eq!(
        request_policy::resolve_expiration(&Retention::default(), u64::MAX, 1)
            .expect_err("default overflow")
            .code,
        FailureCode::InvalidRequest
    );

    let directory = tempfile::tempdir().expect("temp directory");
    let mut invalid = EngineConfig::local(directory.path().join("store.sqlite"));
    invalid.max_store_bytes = 0;
    assert_eq!(
        Engine::fixture(invalid, 1).err().expect("cap").code,
        FailureCode::InvalidRequest
    );
    let mut invalid = EngineConfig::local(directory.path().join("lineage-zero.sqlite"));
    invalid.max_lineage_bytes = 0;
    assert_eq!(
        Engine::fixture(invalid, 1)
            .err()
            .expect("zero lineage cap")
            .code,
        FailureCode::InvalidRequest
    );
    let mut invalid = EngineConfig::local(directory.path().join("lineage-high.sqlite"));
    invalid.max_lineage_bytes = MAX_LINEAGE_BYTES + 1;
    assert_eq!(
        Engine::fixture(invalid, 1)
            .err()
            .expect("high lineage cap")
            .code,
        FailureCode::InvalidRequest
    );

    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let mut overlapping = EngineConfig::local(workspace.join("store.sqlite"));
    overlapping.roots.insert("workspace".to_owned(), workspace);
    assert_eq!(
        Engine::fixture(overlapping, 1)
            .err()
            .expect("overlapping store")
            .code,
        FailureCode::UnsafeRoot
    );

    let mut invalid = EngineConfig::local(directory.path().join("other/store.sqlite"));
    invalid.busy_timeout_ms = 0;
    assert_eq!(
        Engine::fixture(invalid, 1)
            .err()
            .expect("busy timeout")
            .code,
        FailureCode::InvalidRequest
    );
    let mut invalid = EngineConfig::local(directory.path().join("other/store.sqlite"));
    invalid.default_ttl_seconds = 0;
    assert_eq!(
        Engine::fixture(invalid, 1).err().expect("retention").code,
        FailureCode::InvalidRequest
    );
    assert_eq!(
        Engine::fixture(EngineConfig::local("relative.sqlite".into()), 1)
            .err()
            .expect("relative store")
            .code,
        FailureCode::UnsafeRoot
    );
}

#[test]
fn partial_process_capture_is_committed_but_never_projected_as_complete() {
    let directory = tempfile::tempdir().expect("temp directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let mut config = EngineConfig::local(directory.path().join("private/store.sqlite"));
    config.roots.insert("workspace".to_owned(), workspace);
    let engine = Engine::new(config).expect("engine");
    let process = Request {
        contract_version: CONTRACT_VERSION.to_owned(),
        request_id: "timeout".to_owned(),
        source: Source::Process {
            executable: ByteString::from_utf8(
                ["/usr/bin/sleep", "/bin/sleep"]
                    .into_iter()
                    .find(|path| std::path::Path::new(path).exists())
                    .expect("sleep"),
            ),
            argv: vec![ByteString::from_utf8("5")],
            cwd_root_id: "workspace".to_owned(),
            cwd_relative_path: ByteString::default(),
            timeout_ms: Some(100),
            environment_profile: None,
        },
        budget: Budget {
            unit: CountUnit::Bytes,
            total_visible_limit: 64,
            reserved_envelope: 0,
            token_profile: None,
        },
        preservation_profile: "plain-text/v1".to_owned(),
        retention: Retention::default(),
    };
    let failure = engine.handle(process).expect_err("timeout");
    assert_eq!(failure.code, FailureCode::AcquisitionFailed);
    let acquisition = failure.acquisition.expect("partial metadata");
    assert!(acquisition.partial);
    assert!(acquisition.process.expect("process").timed_out);
    let artifact = failure.artifact.expect("partial artifact");

    let mut recovery = request(b"unused", 64);
    recovery.request_id = "recovery".to_owned();
    recovery.source = Source::Artifact {
        artifact: artifact.clone(),
        selector: None,
    };
    let rejected = engine
        .handle(recovery)
        .expect_err("partial artifacts are diagnosis-only");
    assert_eq!(rejected.code, FailureCode::AcquisitionFailed);
    assert_eq!(rejected.artifact, Some(artifact.clone()));
    assert!(rejected.acquisition.expect("partial metadata").partial);

    // US-007: a selector cannot present a partial artifact as a complete region.
    let selected = selected(
        &engine,
        &artifact,
        ArtifactSelector::Lines {
            start_line: 1,
            line_count: 1,
        },
        64,
    )
    .expect_err("partial artifacts stay diagnosis-only under a selector");
    assert_eq!(selected.code, FailureCode::AcquisitionFailed);
    assert!(selected.acquisition.expect("partial metadata").partial);
}

#[test]
fn clean_acquisition_failures_include_safe_deterministic_metadata() {
    let directory = tempfile::tempdir().expect("temp directory");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::write(workspace.join("binary"), [0, 0xff]).expect("binary file");
    let mut config = EngineConfig::local(directory.path().join("private/store.sqlite"));
    config.roots.insert("workspace".to_owned(), workspace);
    let engine = Engine::new(config).expect("engine");

    let mut binary = request(b"unused", 64);
    binary.source = Source::File {
        root_id: "workspace".to_owned(),
        relative_path: ByteString::from_utf8("binary"),
        binary_policy: BinaryPolicy::Reject,
    };
    let binary_failure = engine.handle(binary).expect_err("binary policy");
    assert_eq!(binary_failure.code, FailureCode::AcquisitionFailed);
    let binary_receipt = binary_failure.acquisition.expect("file metadata");
    assert_eq!(binary_receipt.variant, SourceVariant::File);
    assert!(!binary_receipt.complete);
    assert!(
        !binary_receipt
            .relative_path
            .expect("safe path")
            .contains("binary")
    );

    let mut spawn = request(b"unused", 64);
    spawn.source = Source::Process {
        executable: ByteString::from_utf8("/definitely/missing/distill-command"),
        argv: vec![ByteString::from_utf8("$(not-shell)")],
        cwd_root_id: "workspace".to_owned(),
        cwd_relative_path: ByteString::default(),
        timeout_ms: None,
        environment_profile: None,
    };
    let spawn_failure = engine.handle(spawn).expect_err("spawn");
    assert_eq!(spawn_failure.code, FailureCode::AcquisitionFailed);
    let spawn_receipt = spawn_failure.acquisition.expect("process metadata");
    assert_eq!(spawn_receipt.variant, SourceVariant::Process);
    assert!(!spawn_receipt.complete);
    assert_eq!(
        spawn_receipt.process.expect("process").working_directory,
        "workspace:<0 path bytes>"
    );
}

fn committed(engine: &Engine, source: &str) -> ArtifactRef {
    let mut capture = request(source.as_bytes(), source.len() as u64 + 1);
    capture.request_id = "capture".to_owned();
    engine.handle(capture).expect("committed source").artifact
}

fn selected(
    engine: &Engine,
    artifact: &ArtifactRef,
    selector: ArtifactSelector,
    limit: u64,
) -> Result<Outcome, Failure> {
    let mut retrieval = request(b"unused", limit);
    retrieval.request_id = "retrieval".to_owned();
    retrieval.source = Source::Artifact {
        artifact: artifact.clone(),
        selector: Some(selector),
    };
    engine.handle(retrieval)
}

fn covers_exactly(receipt: &Receipt, source_len: usize) {
    let mut coverage = vec![0_u8; source_len];
    for span in receipt
        .retained_spans
        .iter()
        .chain(receipt.omitted_spans.iter())
    {
        for byte in &mut coverage[span.start as usize..span.end as usize] {
            *byte += 1;
        }
    }
    assert!(coverage.iter().all(|count| *count == 1));
}

/// US-007: a line selector returns only its region, projected under the request
/// budget, with a receipt that still partitions the whole committed source.
#[test]
fn a_line_selector_returns_its_region_in_original_source_offsets() {
    let (_directory, engine) = fixture();
    let source = "alpha\nbravo\ncharlie\ndelta\necho\n";
    let artifact = committed(&engine, source);

    let outcome = selected(
        &engine,
        &artifact,
        ArtifactSelector::Lines {
            start_line: 2,
            line_count: 2,
        },
        64,
    )
    .expect("line selection");

    assert_eq!(outcome.visible.bytes, "bravo\ncharlie\n");
    assert_eq!(outcome.receipt.fidelity, Fidelity::Extractive);
    assert_eq!(
        outcome.receipt.retained_spans,
        vec![ByteSpan { start: 6, end: 20 }]
    );
    assert_eq!(
        outcome.receipt.omitted_spans,
        vec![
            ByteSpan { start: 0, end: 6 },
            ByteSpan {
                start: 20,
                end: source.len() as u64
            }
        ]
    );
    // Accounting stays anchored on the complete artifact, so the caller sees
    // everything outside the selection as omitted.
    assert_eq!(outcome.receipt.original_count, source.len() as u64);
    assert_eq!(outcome.receipt.visible_count, 14);
    covers_exactly(&outcome.receipt, source.len());

    // A range past the end of the source is an empty success, not a failure.
    let empty = selected(
        &engine,
        &artifact,
        ArtifactSelector::Lines {
            start_line: 99,
            line_count: 1,
        },
        64,
    )
    .expect("empty selection");
    assert_eq!(empty.visible.bytes, "");
    assert_eq!(empty.receipt.visible_count, 0);
    assert_eq!(empty.receipt.original_count, source.len() as u64);
    covers_exactly(&empty.receipt, source.len());
}

/// US-007: a literal pattern selector returns the matching regions with their
/// configured context, capped at the documented maximum match count.
#[test]
fn a_pattern_selector_returns_matching_regions_with_bounded_context() {
    let (_directory, engine) = fixture();
    let source = "alpha\nbravo\nNEEDLE one\ncharlie\ndelta\nNEEDLE two\necho\n";
    let artifact = committed(&engine, source);

    let contextual = selected(
        &engine,
        &artifact,
        ArtifactSelector::Pattern {
            pattern: ByteString::from_utf8("NEEDLE"),
            before_lines: Some(1),
            after_lines: Some(0),
            max_matches: None,
        },
        256,
    )
    .expect("pattern selection");
    assert_eq!(
        contextual.visible.bytes,
        "bravo\nNEEDLE one\ndelta\nNEEDLE two\n"
    );
    assert_eq!(contextual.receipt.retained_spans.len(), 2);
    assert_eq!(contextual.receipt.original_count, source.len() as u64);
    covers_exactly(&contextual.receipt, source.len());

    let capped = selected(
        &engine,
        &artifact,
        ArtifactSelector::Pattern {
            pattern: ByteString::from_utf8("NEEDLE"),
            before_lines: Some(0),
            after_lines: Some(0),
            max_matches: Some(1),
        },
        256,
    )
    .expect("capped pattern selection");
    assert_eq!(capped.visible.bytes, "NEEDLE one\n");

    // An absent literal selects nothing and stays a typed success.
    let absent = selected(
        &engine,
        &artifact,
        ArtifactSelector::Pattern {
            pattern: ByteString::from_utf8("ABSENT"),
            before_lines: None,
            after_lines: None,
            max_matches: None,
        },
        256,
    )
    .expect("absent literal");
    assert_eq!(absent.visible.bytes, "");
    assert!(absent.receipt.retained_spans.is_empty());
}

/// US-007: an over-budget selection is projected under the request budget, not
/// returned whole, and the receipt still partitions the source exactly.
#[test]
fn an_over_budget_selection_is_projected_under_the_request_budget() {
    let (_directory, engine) = fixture();
    let source = (0..400)
        .map(|index| format!("line {index:03} of the committed observation\n"))
        .collect::<String>();
    let artifact = committed(&engine, &source);

    let outcome = selected(
        &engine,
        &artifact,
        ArtifactSelector::Lines {
            start_line: 100,
            line_count: 200,
        },
        256,
    )
    .expect("over-budget selection");

    assert_eq!(outcome.receipt.fidelity, Fidelity::Extractive);
    assert!(outcome.receipt.visible_count <= 256);
    assert_eq!(outcome.receipt.original_count, source.len() as u64);
    covers_exactly(&outcome.receipt, source.len());
    // Every retained byte comes from inside the selected region.
    let region_start = source.find("line 099 ").expect("selection start") as u64;
    let region_end = source.find("line 299 ").expect("selection end") as u64;
    for span in &outcome.receipt.retained_spans {
        assert!(span.start >= region_start && span.end <= region_end);
    }
}

/// US-007: selector bounds are enforced before any store read, and an artifact
/// whose bytes cannot be selected fails with the documented typed failure.
#[test]
fn selector_bounds_are_rejected_before_any_store_read() {
    let directory = tempfile::tempdir().expect("temp directory");
    let config = EngineConfig::local(directory.path().join("store/store.sqlite"));
    let store_path = config.store_path.clone();
    let engine = Engine::fixture(config, 1_000).expect("fixture engine");
    let artifact = committed(&engine, "alpha\nbravo\n");
    let records_before = engine.status().expect("status").store_records;

    for selector in [
        ArtifactSelector::Lines {
            start_line: 0,
            line_count: 1,
        },
        ArtifactSelector::Lines {
            start_line: 1,
            line_count: 0,
        },
        ArtifactSelector::Pattern {
            pattern: ByteString::default(),
            before_lines: None,
            after_lines: None,
            max_matches: None,
        },
        ArtifactSelector::Pattern {
            pattern: ByteString(vec![b'x'; MAX_SELECTOR_PATTERN_BYTES + 1]),
            before_lines: None,
            after_lines: None,
            max_matches: None,
        },
        ArtifactSelector::Pattern {
            pattern: ByteString(vec![0xff, 0xfe]),
            before_lines: None,
            after_lines: None,
            max_matches: None,
        },
        ArtifactSelector::Pattern {
            pattern: ByteString::from_utf8("alpha"),
            before_lines: Some(MAX_SELECTOR_CONTEXT_LINES + 1),
            after_lines: None,
            max_matches: None,
        },
        ArtifactSelector::Pattern {
            pattern: ByteString::from_utf8("alpha"),
            before_lines: None,
            after_lines: Some(MAX_SELECTOR_CONTEXT_LINES + 1),
            max_matches: None,
        },
        ArtifactSelector::Pattern {
            pattern: ByteString::from_utf8("alpha"),
            before_lines: None,
            after_lines: None,
            max_matches: Some(0),
        },
        ArtifactSelector::Pattern {
            pattern: ByteString::from_utf8("alpha"),
            before_lines: None,
            after_lines: None,
            max_matches: Some(MAX_SELECTOR_MATCHES + 1),
        },
    ] {
        let failure =
            selected(&engine, &artifact, selector, 64).expect_err("out-of-bounds selector");
        assert_eq!(failure.code, FailureCode::InvalidRequest);
        assert_eq!(failure.request_id.as_deref(), Some("retrieval"));
        assert!(failure.artifact.is_none());
    }
    assert_eq!(
        engine.status().expect("status").store_records,
        records_before
    );
    assert!(store_path.exists());

    // Line ranges and literal text have no meaning over arbitrary bytes.
    let mut binary = request(&[0xff, b'\n', 0xfe], 64);
    binary.request_id = "binary".to_owned();
    let binary = engine.handle(binary).expect("binary capture").artifact;
    assert_eq!(
        selected(
            &engine,
            &binary,
            ArtifactSelector::Lines {
                start_line: 1,
                line_count: 1
            },
            64,
        )
        .expect_err("non-UTF-8 selection")
        .code,
        FailureCode::InvalidRequest
    );
}

/// US-007: retrieval reuses the artifact source path, so its expired, unknown,
/// corrupt, and partial failures stay exactly what they already were.
#[test]
fn selection_preserves_the_existing_artifact_failure_contract() {
    let directory = tempfile::tempdir().expect("temp directory");
    let config = EngineConfig::local(directory.path().join("store/store.sqlite"));
    let engine = Engine::fixture(config.clone(), 1_000).expect("fixture engine");
    let slice = || ArtifactSelector::Lines {
        start_line: 1,
        line_count: 1,
    };
    let mut capture = request(b"alpha\nbravo\n", 64);
    capture.request_id = "capture".to_owned();
    capture.retention = Retention {
        expires_at: None,
        ttl_seconds: Some(10),
    };
    let artifact = engine.handle(capture).expect("committed source").artifact;

    let unknown = ArtifactRef {
        id: "0".repeat(32),
        ..artifact.clone()
    };
    assert_eq!(
        selected(&engine, &unknown, slice(), 64)
            .expect_err("unknown artifact")
            .code,
        FailureCode::ArtifactUnknown
    );

    let mismatched = ArtifactRef {
        source_sha256: "b".repeat(64),
        ..artifact.clone()
    };
    assert_eq!(
        selected(&engine, &mismatched, slice(), 64)
            .expect_err("mismatched digest")
            .code,
        FailureCode::ArtifactCorrupt
    );

    let later = Engine::fixture(config, 2_000).expect("engine past expiration");
    assert_eq!(
        selected(&later, &artifact, slice(), 64)
            .expect_err("expired artifact")
            .code,
        FailureCode::ArtifactExpired
    );
}

/// US-007: v3 adds only the optional selector, so a v2 request without one is
/// unchanged and a v2 request with one is refused rather than silently honored.
#[test]
fn the_previous_contract_version_stays_accepted_without_a_selector() {
    let (_directory, engine) = fixture();
    let artifact = committed(&engine, "alpha\nbravo\n");

    let mut legacy = request(b"unused", 64);
    legacy.contract_version = CONTRACT_VERSION_V2.to_owned();
    legacy.request_id = "legacy".to_owned();
    legacy.source = Source::Artifact {
        artifact: artifact.clone(),
        selector: None,
    };
    let outcome = engine.handle(legacy.clone()).expect("v2 artifact request");
    assert_eq!(outcome.visible.bytes, "alpha\nbravo\n");
    assert_eq!(outcome.receipt.fidelity, Fidelity::Exact);

    legacy.source = Source::Artifact {
        artifact,
        selector: Some(ArtifactSelector::Lines {
            start_line: 1,
            line_count: 1,
        }),
    };
    assert_eq!(
        engine.handle(legacy).expect_err("v2 selector").code,
        FailureCode::SchemaUnsupported
    );
}

/// US-007: a selection over the largest accepted observation stays inside the
/// documented retrieval deadline and allocates a bounded multiple of its match
/// payload.
#[test]
fn selection_over_the_largest_observation_stays_within_its_envelope() {
    let (_directory, engine) = fixture();
    let mut source = String::with_capacity(MAX_SOURCE_BYTES);
    while source.len() < MAX_SOURCE_BYTES - 64 {
        source.push_str("ordinary observation line without the literal\n");
    }
    source.push_str("DISTILL_RETRIEVAL_NEEDLE tail\n");
    source.truncate(MAX_SOURCE_BYTES);
    let artifact = committed(&engine, &source);

    let started = std::time::Instant::now();
    let outcome = selected(
        &engine,
        &artifact,
        ArtifactSelector::Pattern {
            pattern: ByteString::from_utf8("DISTILL_RETRIEVAL_NEEDLE"),
            before_lines: Some(MAX_SELECTOR_CONTEXT_LINES),
            after_lines: Some(MAX_SELECTOR_CONTEXT_LINES),
            max_matches: Some(MAX_SELECTOR_MATCHES),
        },
        4_096,
    )
    .expect("large artifact search");
    let elapsed = started.elapsed();

    assert!(outcome.visible.bytes.contains("DISTILL_RETRIEVAL_NEEDLE"));
    assert!(outcome.receipt.visible_count <= 4_096);
    // At most 32 matches of at most 33 lines each bounds the selected payload
    // far below the source, whatever the artifact size.
    let selected_bytes: u64 = outcome
        .receipt
        .retained_spans
        .iter()
        .map(|span| span.end - span.start)
        .sum();
    assert!(selected_bytes <= 4_096);
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "retrieval over {} bytes took {elapsed:?}",
        source.len()
    );
}

#[test]
fn failures_and_receipts_never_copy_raw_source() {
    let (_directory, engine) = fixture();
    let secret = "DISTILL_TEST_SECRET_5f19";
    let mut candidate = request(format!("ERROR {secret}: mandatory\n").as_bytes(), 1);
    candidate.preservation_profile = "build-log/v1".to_owned();
    let failure = engine.handle(candidate).expect_err("budget failure");
    let encoded = serde_json::to_string(&failure).expect("failure JSON");
    assert!(!encoded.contains(secret));
    assert!(!failure.safe_message.contains(secret));
}
