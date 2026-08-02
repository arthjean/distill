use super::*;
use std::collections::BTreeSet;
use std::os::unix::fs::{PermissionsExt, symlink};
use tempfile::TempDir;

fn invoke(arguments: Vec<String>) -> Result<Value, SurfaceError> {
    let mut output = Vec::new();
    run(arguments.into(), &mut output)?;
    serde_json::from_slice(&output).map_err(|_| SurfaceError::invalid("test output is not JSON"))
}

#[test]
fn codex_setup_conforms_to_adapter_and_versioned_fixture() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../docs/integrations/codex-hook-conformance-v1.json"
    ))
    .expect("conformance fixture");
    assert_eq!(fixture["adapter_input_version"], codex::HOOK_SCHEMA_VERSION);
    assert_eq!(
        fixture["host_output_cap"]["documented_approximate_tokens"],
        codex::HOST_OUTPUT_CAP_TOKENS
    );
    assert_eq!(
        fixture["host_output_cap"]["distill_maximum_tokens"],
        codex::SAFE_OUTPUT_CAP_TOKENS
    );
    assert_eq!(fixture["setup"]["matcher"], codex::SETUP_MATCHER);
    assert_eq!(
        fixture["setup"]["timeout_seconds"],
        codex::SETUP_TIMEOUT_SECONDS
    );
    assert_eq!(
        fixture["setup"]["status_message"],
        codex::SETUP_STATUS_MESSAGE
    );
    assert_eq!(
        fixture["setup"]["command_arguments"],
        json!(codex::setup_hook_arguments(codex::Mode::Active))
    );
    let fixture_modes = fixture["modes"]
        .as_object()
        .expect("fixture modes")
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        fixture_modes,
        codex::SUPPORTED_MODES.iter().copied().collect()
    );
    for surface in fixture["surfaces"].as_array().expect("fixture surfaces") {
        let status = surface["status"].as_str().expect("surface status");
        if status == "supported" {
            assert!(
                !codex::is_unsupported_surface(
                    surface["fixture"].as_str().expect("supported fixture")
                ),
                "{}",
                surface["category"]
            );
        } else if status == "diagnostic" {
            assert!(codex::is_unsupported_surface(
                surface["fixture"].as_str().expect("diagnostic fixture")
            ));
            assert_eq!(surface["matcher"], codex::SETUP_MATCHER);
            assert_eq!(surface["diagnostic"], "unsupported_surface");
        } else {
            assert_eq!(status, "no_event");
            assert!(surface["matcher"].is_null());
            assert!(surface.get("fixture").is_none());
        }
    }

    let temp = TempDir::new().expect("temp");
    let config = temp.path().join("codex.json");
    let receipt = invoke(vec![
        "codex".to_owned(),
        "--config".to_owned(),
        config.display().to_string(),
        "--command".to_owned(),
        "distill".to_owned(),
        "--mode".to_owned(),
        "active".to_owned(),
        "--dry-run".to_owned(),
    ])
    .expect("Codex dry run");
    let entry = &receipt["managed_entry"];
    assert_eq!(entry["matcher"], fixture["setup"]["matcher"]);
    assert_eq!(
        entry["hooks"][0]["timeout"],
        fixture["setup"]["timeout_seconds"]
    );
    assert_eq!(
        entry["hooks"][0]["statusMessage"],
        fixture["setup"]["status_message"]
    );
    assert_eq!(
        entry["hooks"][0]["command"],
        "'distill' 'codex-hook' '--mode' 'active'"
    );
    assert_eq!(receipt["action"], "dry_run");
    assert!(!config.exists());
}

#[test]
fn codex_setup_is_dry_runnable_idempotent_and_exactly_reversible() {
    let temp = TempDir::new().expect("temp");
    let config = temp.path().join("codex/hooks.json");
    fs::create_dir_all(config.parent().expect("parent")).expect("directory");
    let original = b"{\n  \"description\": \"keep me\",\n  \"hooks\": {}\n}\n";
    fs::write(&config, original).expect("original");
    let arguments = vec![
        "codex".to_owned(),
        "--config".to_owned(),
        config.display().to_string(),
        "--command".to_owned(),
        "/opt/distill's bin".to_owned(),
        "--store".to_owned(),
        temp.path().join("store.db").display().to_string(),
    ];
    let mut dry = arguments.clone();
    dry.push("--dry-run".to_owned());
    let receipt = invoke(dry).expect("dry run");
    assert_eq!(receipt["action"], "dry_run");
    assert!(receipt.get("configuration").is_none());
    assert!(receipt["managed_entry"].is_object());
    assert_eq!(fs::read(&config).expect("unchanged"), original);
    assert!(!backup_path(&config).exists());

    let receipt = invoke(arguments.clone()).expect("install");
    assert_eq!(receipt["action"], "installed");
    let installed = fs::read(&config).expect("installed bytes");
    let installed_json: Value = serde_json::from_slice(&installed).expect("installed JSON");
    assert!(
        installed_json["hooks"]["PostToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .expect("hook command")
            .contains("'\"'\"'")
    );
    assert_eq!(fs::read(backup_path(&config)).expect("backup"), original);
    assert_eq!(
        fs::metadata(&config)
            .expect("config metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(backup_path(&config))
            .expect("backup metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    let receipt = invoke(arguments).expect("idempotent");
    assert_eq!(receipt["action"], "unchanged");
    let receipt = invoke(vec![
        "codex".to_owned(),
        "--config".to_owned(),
        config.display().to_string(),
        "--restore".to_owned(),
    ])
    .expect("restore");
    assert_eq!(receipt["action"], "restored");
    assert_eq!(fs::read(&config).expect("restored bytes"), original);
}

#[test]
fn claude_setup_preserves_other_servers_and_restores_absence() {
    let temp = TempDir::new().expect("temp");
    let config = temp.path().join("claude/settings.json");
    let root = temp.path().join("workspace");
    let receipt = invoke(vec![
        "claude".to_owned(),
        "--config".to_owned(),
        config.display().to_string(),
        "--command".to_owned(),
        "/opt/distill".to_owned(),
        "--root".to_owned(),
        format!("workspace={}", root.display()),
    ])
    .expect("install");
    assert_eq!(receipt["action"], "installed");
    let document: Value =
        serde_json::from_slice(&fs::read(&config).expect("config")).expect("JSON");
    assert_eq!(document["mcpServers"]["distill"]["command"], "/opt/distill");
    assert!(
        document["mcpServers"]["distill"]["args"]
            .as_array()
            .expect("args")
            .contains(&json!("mcp"))
    );
    assert!(absent_path(&config).exists());

    let receipt = invoke(vec![
        "claude".to_owned(),
        "--config".to_owned(),
        config.display().to_string(),
        "--restore".to_owned(),
    ])
    .expect("restore absent");
    assert_eq!(receipt["action"], "restored_absent");
    assert!(!config.exists());
}

#[test]
fn setup_rejects_malformed_symlinked_and_ambiguous_targets() {
    let temp = TempDir::new().expect("temp");
    let malformed = temp.path().join("malformed.json");
    fs::write(&malformed, b"not json").expect("malformed");
    let result = invoke(vec![
        "codex".to_owned(),
        "--config".to_owned(),
        malformed.display().to_string(),
        "--command".to_owned(),
        "distill".to_owned(),
    ]);
    assert!(result.is_err());
    assert!(!backup_path(&malformed).exists());

    let real = temp.path().join("real.json");
    fs::write(&real, b"{}").expect("real");
    let linked = temp.path().join("linked.json");
    symlink(&real, &linked).expect("symlink");
    let result = invoke(vec![
        "claude".to_owned(),
        "--config".to_owned(),
        linked.display().to_string(),
        "--command".to_owned(),
        "distill".to_owned(),
    ]);
    assert!(result.is_err());
    fs::write(absent_path(&linked), ABSENT_MARKER).expect("absent marker");
    let result = invoke(vec![
        "claude".to_owned(),
        "--config".to_owned(),
        linked.display().to_string(),
        "--restore".to_owned(),
    ]);
    assert!(result.is_err());
    assert!(
        fs::symlink_metadata(&linked)
            .expect("preserved symlink")
            .file_type()
            .is_symlink()
    );

    let result = invoke(vec![
        "codex".to_owned(),
        "--config".to_owned(),
        real.display().to_string(),
        "--command".to_owned(),
        "distill".to_owned(),
        "--dry-run".to_owned(),
        "--restore".to_owned(),
    ]);
    assert!(result.is_err());
}

#[test]
fn setup_rejects_invalid_shapes_and_updates_only_owned_entries() {
    let temp = TempDir::new().expect("temp");
    assert!(invoke(vec!["other".to_owned()]).is_err());

    let error = invoke(vec![
        "codex".to_owned(),
        "--mode".to_owned(),
        "automatic".to_owned(),
    ])
    .expect_err("config validation precedes target validation");
    assert_eq!(error.message, "--config is required");

    let error = invoke(vec![
        "codex".to_owned(),
        "--config".to_owned(),
        "relative.json".to_owned(),
    ])
    .expect_err("path validation precedes command validation");
    assert_eq!(error.message, "--config must be a normalized absolute path");

    let config = temp.path().join("invalid-mode.json");
    assert!(
        invoke(vec![
            "codex".to_owned(),
            "--config".to_owned(),
            config.display().to_string(),
            "--command".to_owned(),
            "distill".to_owned(),
            "--mode".to_owned(),
            "automatic".to_owned(),
        ])
        .is_err()
    );

    assert!(
        invoke(vec![
            "codex".to_owned(),
            "--config".to_owned(),
            "relative.json".to_owned(),
            "--command".to_owned(),
            "distill".to_owned(),
        ])
        .is_err()
    );

    let hooks_not_object = temp.path().join("hooks-not-object.json");
    fs::write(&hooks_not_object, b"{\"hooks\":[]}").expect("invalid hooks");
    assert!(
        invoke(vec![
            "codex".to_owned(),
            "--config".to_owned(),
            hooks_not_object.display().to_string(),
            "--command".to_owned(),
            "distill".to_owned(),
        ])
        .is_err()
    );

    let groups_not_array = temp.path().join("groups-not-array.json");
    fs::write(&groups_not_array, b"{\"hooks\":{\"PostToolUse\":{}}}").expect("invalid groups");
    assert!(
        invoke(vec![
            "codex".to_owned(),
            "--config".to_owned(),
            groups_not_array.display().to_string(),
            "--command".to_owned(),
            "distill".to_owned(),
        ])
        .is_err()
    );

    let claude = temp.path().join("claude.json");
    assert!(
        invoke(vec![
            "claude".to_owned(),
            "--config".to_owned(),
            claude.display().to_string(),
            "--command".to_owned(),
            "distill".to_owned(),
            "--root".to_owned(),
            "workspace=relative".to_owned(),
        ])
        .is_err()
    );

    let codex = temp.path().join("owned-hooks.json");
    let base = vec![
        "codex".to_owned(),
        "--config".to_owned(),
        codex.display().to_string(),
        "--command".to_owned(),
        "distill".to_owned(),
    ];
    invoke(base.clone()).expect("initial hook");
    let mut changed = base;
    changed.extend(["--mode".to_owned(), "observe".to_owned()]);
    let receipt = invoke(changed).expect("updated hook");
    assert_eq!(receipt["action"], "installed");
    assert!(
        receipt["managed_entry"]["hooks"][0]["command"]
            .as_str()
            .expect("managed command")
            .contains("'observe'")
    );
}

#[test]
fn setup_rejects_target_inapplicable_options_and_duplicate_roots() {
    let temp = TempDir::new().expect("temp");
    let codex_config = temp.path().join("codex.json");
    assert!(
        invoke(vec![
            "codex".to_owned(),
            "--config".to_owned(),
            codex_config.display().to_string(),
            "--command".to_owned(),
            "distill".to_owned(),
            "--root".to_owned(),
            format!("workspace={}", temp.path().display()),
        ])
        .is_err()
    );
    assert!(!codex_config.exists());

    let claude_config = temp.path().join("claude.json");
    assert!(
        invoke(vec![
            "claude".to_owned(),
            "--config".to_owned(),
            claude_config.display().to_string(),
            "--command".to_owned(),
            "distill".to_owned(),
            "--mode".to_owned(),
            "active".to_owned(),
        ])
        .is_err()
    );
    assert!(
        invoke(vec![
            "claude".to_owned(),
            "--config".to_owned(),
            claude_config.display().to_string(),
            "--command".to_owned(),
            "distill".to_owned(),
            "--root".to_owned(),
            format!("workspace={}", temp.path().display()),
            "--root".to_owned(),
            format!("workspace={}", temp.path().join("other").display()),
        ])
        .is_err()
    );
    assert!(!claude_config.exists());
}

#[test]
fn duplicate_codex_entries_and_failed_replacements_preserve_original_bytes() {
    let temp = TempDir::new().expect("temp");
    let duplicate = temp.path().join("duplicate.json");
    let duplicate_bytes = format!(
        r#"{{"hooks":{{"PostToolUse":[
                {{"hooks":[{{"statusMessage":"{}"}}]}},
                {{"hooks":[{{"statusMessage":"{}"}}]}}
            ]}}}}"#,
        codex::SETUP_STATUS_MESSAGE,
        codex::SETUP_STATUS_MESSAGE
    );
    fs::write(&duplicate, duplicate_bytes.as_bytes()).expect("duplicate config");
    assert!(
        invoke(vec![
            "codex".to_owned(),
            "--config".to_owned(),
            duplicate.display().to_string(),
            "--command".to_owned(),
            "distill".to_owned(),
        ])
        .is_err()
    );
    assert_eq!(
        fs::read(&duplicate).expect("preserved duplicate config"),
        duplicate_bytes.as_bytes()
    );
    assert!(!backup_path(&duplicate).exists());

    let ambiguous = temp.path().join("ambiguous.json");
    let ambiguous_original = b"{\"preserve\":true}";
    fs::write(&ambiguous, ambiguous_original).expect("ambiguous config");
    fs::write(backup_path(&ambiguous), b"prior").expect("backup marker");
    fs::write(absent_path(&ambiguous), ABSENT_MARKER).expect("absent marker");
    assert!(
        invoke(vec![
            "claude".to_owned(),
            "--config".to_owned(),
            ambiguous.display().to_string(),
            "--command".to_owned(),
            "distill".to_owned(),
        ])
        .is_err()
    );
    assert_eq!(
        fs::read(&ambiguous).expect("preserved ambiguous config"),
        ambiguous_original
    );

    for target in ["codex", "claude"] {
        let config = temp.path().join(format!("{target}-atomic.json"));
        let original = br#"{"keep":"exact bytes"}"#;
        fs::write(&config, original).expect("original config");
        let temporary = config.with_file_name(format!(
            ".{}.distill-tmp-{}",
            config
                .file_name()
                .and_then(|name| name.to_str())
                .expect("name"),
            std::process::id()
        ));
        fs::write(&temporary, b"occupied").expect("occupied temporary");
        assert!(
            invoke(vec![
                target.to_owned(),
                "--config".to_owned(),
                config.display().to_string(),
                "--command".to_owned(),
                "distill".to_owned(),
            ])
            .is_err()
        );
        assert_eq!(fs::read(&config).expect("preserved config"), original);
    }
}
