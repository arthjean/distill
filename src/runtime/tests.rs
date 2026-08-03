use super::*;
use crate::types::{BinaryPolicy, EngineConfig};
use std::os::unix::fs::symlink;
use tempfile::TempDir;

fn runtime() -> (TempDir, ProductionRuntime) {
    let directory = tempfile::tempdir().expect("temp directory");
    let mut config = EngineConfig::local(directory.path().join("store/store.sqlite"));
    config
        .roots
        .insert("workspace".to_owned(), directory.path().to_path_buf());
    config.environment_profiles.insert(
        "safe".to_owned(),
        BTreeMap::from([("DISTILL_SAFE".to_owned(), "value".to_owned())]),
    );
    let runtime = ProductionRuntime::new(&config).expect("runtime");
    (directory, runtime)
}

#[test]
fn production_runtime_rejects_each_invalid_configuration_shape() {
    let directory = tempfile::tempdir().expect("temp directory");
    let base = EngineConfig::local(directory.path().join("store.sqlite"));

    let mut invalid_root_id = base.clone();
    invalid_root_id
        .roots
        .insert(String::new(), directory.path().to_path_buf());
    assert_eq!(
        ProductionRuntime::new(&invalid_root_id)
            .expect_err("invalid root ID")
            .safe_message,
        "root ID is invalid"
    );

    let mut invalid_profile_id = base.clone();
    invalid_profile_id
        .environment_profiles
        .insert(String::new(), BTreeMap::new());
    assert_eq!(
        ProductionRuntime::new(&invalid_profile_id)
            .expect_err("invalid profile ID")
            .safe_message,
        "environment profile ID is invalid"
    );

    let too_many_variables = (0..129)
        .map(|index| (format!("VARIABLE_{index}"), String::new()))
        .collect();
    let invalid_variables = [
        too_many_variables,
        BTreeMap::from([(String::new(), String::new())]),
        BTreeMap::from([("K".repeat(129), String::new())]),
        BTreeMap::from([("VARIABLE".to_owned(), "v".repeat(4_097))]),
        BTreeMap::from([("INVALID=VARIABLE".to_owned(), String::new())]),
        BTreeMap::from([("INVALID\0VARIABLE".to_owned(), String::new())]),
        BTreeMap::from([("VARIABLE".to_owned(), "invalid\0value".to_owned())]),
    ];

    for variables in invalid_variables {
        let mut config = base.clone();
        config
            .environment_profiles
            .insert("profile".to_owned(), variables);
        assert_eq!(
            ProductionRuntime::new(&config)
                .expect_err("invalid environment profile")
                .code,
            FailureCode::InvalidRequest
        );
    }
}

#[test]
fn inline_capture_preserves_arbitrary_bytes_and_enforces_limit() {
    let (_directory, runtime) = runtime();
    let source = Source::Inline {
        bytes: ByteString::from(&[0, 0xff, b'x'][..]),
        media_type: None,
    };
    let acquired = runtime.acquire_source(&source).expect("inline");
    assert_eq!(acquired.bytes, [0, 0xff, b'x']);

    let oversized = Source::Inline {
        bytes: ByteString(vec![0; MAX_SOURCE_BYTES + 1]),
        media_type: None,
    };
    assert_eq!(
        runtime
            .acquire_source(&oversized)
            .expect_err("oversized")
            .failure
            .code,
        FailureCode::InputTooLarge
    );
}

#[test]
fn file_capture_is_descriptor_relative_and_binary_aware() {
    let (directory, runtime) = runtime();
    fs::create_dir(directory.path().join("nested")).expect("nested");
    fs::write(directory.path().join("nested/data.bin"), [0, 0xff, b'x']).expect("file");
    let source = Source::File {
        root_id: "workspace".to_owned(),
        relative_path: ByteString::from_utf8("nested/data.bin"),
        binary_policy: BinaryPolicy::Accept,
    };
    let acquired = runtime.acquire_source(&source).expect("file");
    assert_eq!(acquired.bytes, [0, 0xff, b'x']);
    let receipt = acquired.receipt.to_receipt();
    assert_eq!(receipt.root_id.as_deref(), Some("workspace"));
    assert!(
        !receipt
            .relative_path
            .as_ref()
            .expect("safe path")
            .contains("data.bin")
    );

    let mut rejected = source;
    if let Source::File { binary_policy, .. } = &mut rejected {
        *binary_policy = BinaryPolicy::Reject;
    }
    assert_eq!(
        runtime
            .acquire_source(&rejected)
            .expect_err("binary")
            .failure
            .code,
        FailureCode::AcquisitionFailed
    );
}

#[test]
fn file_capture_rejects_unknown_non_regular_oversized_and_invalid_utf8_sources() {
    let (directory, runtime) = runtime();

    let unknown = Source::File {
        root_id: "unknown".to_owned(),
        relative_path: ByteString::from_utf8("data"),
        binary_policy: BinaryPolicy::Accept,
    };
    assert_eq!(
        runtime
            .acquire_source(&unknown)
            .expect_err("unknown root")
            .failure
            .code,
        FailureCode::UnsafeRoot
    );

    fs::create_dir(directory.path().join("directory")).expect("directory");
    let non_regular = Source::File {
        root_id: "workspace".to_owned(),
        relative_path: ByteString::from_utf8("directory"),
        binary_policy: BinaryPolicy::Accept,
    };
    assert_eq!(
        runtime
            .acquire_source(&non_regular)
            .expect_err("non-regular source")
            .failure
            .code,
        FailureCode::AcquisitionFailed
    );

    let oversized_path = directory.path().join("oversized");
    let oversized = File::create(&oversized_path).expect("oversized file");
    oversized
        .set_len(MAX_SOURCE_BYTES as u64 + 1)
        .expect("sparse oversized file");
    let oversized_source = Source::File {
        root_id: "workspace".to_owned(),
        relative_path: ByteString::from_utf8("oversized"),
        binary_policy: BinaryPolicy::Accept,
    };
    assert_eq!(
        runtime
            .acquire_source(&oversized_source)
            .expect_err("oversized source")
            .failure
            .code,
        FailureCode::InputTooLarge
    );

    fs::write(directory.path().join("invalid-utf8"), [0xff]).expect("invalid UTF-8");
    let invalid_utf8 = Source::File {
        root_id: "workspace".to_owned(),
        relative_path: ByteString::from_utf8("invalid-utf8"),
        binary_policy: BinaryPolicy::Reject,
    };
    assert_eq!(
        runtime
            .acquire_source(&invalid_utf8)
            .expect_err("invalid UTF-8")
            .failure
            .code,
        FailureCode::AcquisitionFailed
    );

    assert_eq!(
        runtime
            .acquire_source(&Source::Artifact {
                selector: None,
                artifact: crate::types::ArtifactRef {
                    schema_version: crate::types::ARTIFACT_SCHEMA_VERSION.to_owned(),
                    id: "a".repeat(32),
                    source_sha256: "b".repeat(64),
                    source_bytes: 0,
                    created_at: 1,
                    expires_at: 2,
                },
            })
            .expect_err("artifact source")
            .failure
            .code,
        FailureCode::SourceUnsupported
    );
}

#[test]
fn traversal_symlinks_and_embedded_nul_fail_closed() {
    let (directory, runtime) = runtime();
    fs::write(directory.path().join("safe.txt"), b"safe").expect("safe");
    symlink("/etc/passwd", directory.path().join("escape")).expect("symlink");
    for path in [
        ByteString::from_utf8("../outside"),
        ByteString::from_utf8("/absolute"),
        ByteString::from_utf8("escape"),
        ByteString(b"safe\0.txt".to_vec()),
    ] {
        let source = Source::File {
            root_id: "workspace".to_owned(),
            relative_path: path,
            binary_policy: BinaryPolicy::Accept,
        };
        let code = runtime
            .acquire_source(&source)
            .expect_err("unsafe")
            .failure
            .code;
        assert!(
            matches!(
                code,
                FailureCode::InvalidRequest
                    | FailureCode::UnsafeRoot
                    | FailureCode::AcquisitionFailed
            ),
            "unexpected code: {code:?}"
        );
    }
}

#[test]
fn symlink_replacement_after_root_configuration_never_escapes() {
    let parent = tempfile::tempdir().expect("temp directory");
    let allowed = parent.path().join("allowed");
    let outside = parent.path().join("outside");
    fs::create_dir_all(allowed.join("nested")).expect("allowed");
    fs::create_dir_all(&outside).expect("outside");
    fs::write(allowed.join("nested/data"), b"safe").expect("safe");
    fs::write(outside.join("data"), b"secret").expect("secret");
    let mut config = EngineConfig::local(parent.path().join("store/store.sqlite"));
    config.roots.insert("workspace".to_owned(), allowed.clone());
    let runtime = ProductionRuntime::new(&config).expect("runtime");

    fs::rename(allowed.join("nested"), allowed.join("nested-original")).expect("move original");
    symlink(&outside, allowed.join("nested")).expect("swap symlink");
    let source = Source::File {
        root_id: "workspace".to_owned(),
        relative_path: ByteString::from_utf8("nested/data"),
        binary_policy: BinaryPolicy::Accept,
    };
    assert_eq!(
        runtime
            .acquire_source(&source)
            .expect_err("swapped")
            .failure
            .code,
        FailureCode::UnsafeRoot
    );

    fs::remove_file(allowed.join("nested")).expect("remove symlink");
    fs::rename(allowed.join("nested-original"), allowed.join("nested")).expect("restore");
    assert_eq!(
        runtime.acquire_source(&source).expect("safe").bytes,
        b"safe"
    );

    fs::rename(&allowed, parent.path().join("allowed-original")).expect("move root");
    symlink(&outside, &allowed).expect("root swap");
    assert_eq!(
        runtime
            .acquire_source(&source)
            .expect_err("root replaced")
            .failure
            .code,
        FailureCode::UnsafeRoot
    );
}

#[test]
fn symlinked_or_foreign_root_configuration_is_rejected() {
    let directory = tempfile::tempdir().expect("temp directory");
    let actual = directory.path().join("actual");
    fs::create_dir(&actual).expect("actual");
    let linked = directory.path().join("linked");
    symlink(&actual, &linked).expect("linked");
    let mut config = EngineConfig::local(directory.path().join("store.sqlite"));
    config.roots.insert("root".to_owned(), linked);
    assert_eq!(
        ProductionRuntime::new(&config)
            .expect_err("symlink root")
            .code,
        FailureCode::UnsafeRoot
    );
}

#[test]
fn argv_is_literal_environment_is_allowlisted_and_no_shell_is_inferred() {
    let (directory, runtime) = runtime();
    let marker = directory.path().join("must-not-exist");
    let injection = format!("$(touch {})", marker.display());
    let source = Source::Process {
        executable: ByteString::from_utf8(command_path(&["/usr/bin/printf", "/bin/printf"])),
        argv: vec![
            ByteString::from_utf8("%s\n"),
            ByteString::from_utf8("--literal"),
            ByteString::from_utf8("value with spaces"),
            ByteString::from_utf8(injection.clone()),
        ],
        cwd_root_id: "workspace".to_owned(),
        cwd_relative_path: ByteString::default(),
        timeout_ms: Some(2_000),
        environment_profile: Some("safe".to_owned()),
    };
    let acquired = runtime.acquire_source(&source).expect("process");
    let text = String::from_utf8(acquired.bytes).expect("utf8");
    assert!(text.contains("--literal"));
    assert!(text.contains("value with spaces"));
    assert!(text.contains(&injection));
    assert!(!marker.exists());
    let receipt = acquired.receipt.to_receipt();
    let process = receipt.process.as_ref().expect("process receipt");
    assert_eq!(process.exit_code, Some(0));
    assert_eq!(process.signal, None);
    assert!(!process.timed_out);
    assert!(!process.events.is_empty());
}

#[test]
fn timeout_signal_and_output_limit_return_partial_typed_failures() {
    let (_directory, runtime) = runtime();
    let timeout = Source::Process {
        executable: ByteString::from_utf8(command_path(&["/usr/bin/sleep", "/bin/sleep"])),
        argv: vec![ByteString::from_utf8("5")],
        cwd_root_id: "workspace".to_owned(),
        cwd_relative_path: ByteString::default(),
        timeout_ms: Some(100),
        environment_profile: None,
    };
    let timed_out = runtime.acquire_source(&timeout).expect_err("timeout");
    assert_eq!(timed_out.failure.code, FailureCode::AcquisitionFailed);
    let partial = timed_out.partial.expect("timeout receipt");
    let receipt = partial.receipt.to_receipt();
    assert!(receipt.partial);
    assert!(receipt.process.as_ref().expect("process").timed_out);

    let closed_streams = Source::Process {
        executable: ByteString::from_utf8(command_path(&["/bin/sh", "/usr/bin/sh"])),
        argv: vec![
            ByteString::from_utf8("-c"),
            ByteString::from_utf8("exec 1>&- 2>&-; sleep 5"),
        ],
        cwd_root_id: "workspace".to_owned(),
        cwd_relative_path: ByteString::default(),
        timeout_ms: Some(100),
        environment_profile: None,
    };
    let timed_out = runtime
        .acquire_source(&closed_streams)
        .expect_err("closed streams still obey timeout");
    assert!(
        timed_out
            .partial
            .expect("closed stream receipt")
            .receipt
            .to_receipt()
            .process
            .as_ref()
            .expect("process")
            .timed_out
    );

    let signaled = Source::Process {
        executable: ByteString::from_utf8(command_path(&["/bin/sh", "/usr/bin/sh"])),
        argv: vec![
            ByteString::from_utf8("-c"),
            ByteString::from_utf8("kill -TERM $$"),
        ],
        cwd_root_id: "workspace".to_owned(),
        cwd_relative_path: ByteString::default(),
        timeout_ms: Some(2_000),
        environment_profile: None,
    };
    let killed = runtime.acquire_source(&signaled).expect_err("signal");
    assert_eq!(killed.failure.code, FailureCode::AcquisitionFailed);
    assert!(
        killed
            .partial
            .expect("signal receipt")
            .receipt
            .to_receipt()
            .process
            .as_ref()
            .expect("process")
            .signal
            .is_some()
    );

    let chatter = Source::Process {
        executable: ByteString::from_utf8(command_path(&["/usr/bin/yes", "/bin/yes"])),
        argv: Vec::new(),
        cwd_root_id: "workspace".to_owned(),
        cwd_relative_path: ByteString::default(),
        timeout_ms: Some(5_000),
        environment_profile: None,
    };
    let exhausted = runtime.acquire_source(&chatter).expect_err("output cap");
    assert_eq!(exhausted.failure.code, FailureCode::ResourceExhausted);
    let partial = exhausted.partial.expect("partial output");
    assert_eq!(partial.bytes.len(), MAX_SOURCE_BYTES);
    let receipt = partial.receipt.to_receipt();
    assert!(receipt.truncated);
    assert!(!receipt.complete);
}

#[test]
fn process_request_validation_is_bounded_and_deterministic() {
    let (_directory, runtime) = runtime();
    let invalid_timeout = Source::Process {
        executable: ByteString::from_utf8("/usr/bin/true"),
        argv: Vec::new(),
        cwd_root_id: "workspace".to_owned(),
        cwd_relative_path: ByteString::default(),
        timeout_ms: Some(99),
        environment_profile: None,
    };
    assert_eq!(
        runtime
            .acquire_source(&invalid_timeout)
            .expect_err("timeout range")
            .failure
            .code,
        FailureCode::InvalidRequest
    );

    let nul = Source::Process {
        executable: ByteString(b"/bin/tr\0ue".to_vec()),
        argv: Vec::new(),
        cwd_root_id: "workspace".to_owned(),
        cwd_relative_path: ByteString::default(),
        timeout_ms: None,
        environment_profile: None,
    };
    assert_eq!(
        runtime.acquire_source(&nul).expect_err("nul").failure.code,
        FailureCode::InvalidRequest
    );

    let unknown_environment = Source::Process {
        executable: ByteString::from_utf8("/usr/bin/true"),
        argv: Vec::new(),
        cwd_root_id: "workspace".to_owned(),
        cwd_relative_path: ByteString::default(),
        timeout_ms: None,
        environment_profile: Some("unknown".to_owned()),
    };
    assert_eq!(
        runtime
            .acquire_source(&unknown_environment)
            .expect_err("environment")
            .failure
            .code,
        FailureCode::InvalidRequest
    );
}

#[test]
fn production_ids_are_opaque_and_fixture_ids_are_stable() {
    let (_directory, runtime) = runtime();
    let first = runtime.random_id().expect("random ID");
    let second = runtime.random_id().expect("random ID");
    assert_eq!(first.len(), 32);
    assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_ne!(first, second);

    let fixture = FixtureRuntime::new(42);
    assert_eq!(fixture.now().expect("clock"), 42);
    assert_eq!(
        fixture.random_id().expect("fixture ID"),
        "00000000000000000000000000000001"
    );
}

fn command_path(candidates: &[&str]) -> String {
    candidates
        .iter()
        .find(|path| Path::new(path).exists())
        .expect("required test command")
        .to_string()
}
