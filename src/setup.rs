use crate::cli::{SurfaceError, write_json_line};
use serde_json::{Map, Value, json};
use std::{
    collections::VecDeque,
    fs::{self, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
};

const SETUP_SCHEMA_VERSION: &str = "distill.setup/v1";
const CODEX_SENTINEL: &str = "Distill context projection v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Codex,
    Claude,
}

#[derive(Debug)]
struct Options {
    target: Target,
    config_path: PathBuf,
    command: Option<String>,
    store_path: Option<PathBuf>,
    roots: Vec<String>,
    mode: String,
    dry_run: bool,
    restore: bool,
}

pub(crate) fn run<W: Write>(
    mut args: VecDeque<String>,
    output: &mut W,
) -> Result<(), SurfaceError> {
    let target = match args.pop_front().as_deref() {
        Some("codex") => Target::Codex,
        Some("claude") => Target::Claude,
        _ => return Err(SurfaceError::invalid("setup requires codex or claude")),
    };
    let mut config_path = None;
    let mut command = None;
    let mut store_path = None;
    let mut roots = Vec::new();
    let mut mode = "active".to_owned();
    let mut dry_run = false;
    let mut restore = false;
    while let Some(argument) = args.pop_front() {
        match argument.as_str() {
            "--config" => config_path = Some(PathBuf::from(take(&mut args, "--config")?)),
            "--command" => command = Some(take(&mut args, "--command")?),
            "--store" => store_path = Some(PathBuf::from(take(&mut args, "--store")?)),
            "--root" => roots.push(take(&mut args, "--root")?),
            "--mode" => {
                mode = take(&mut args, "--mode")?;
                if !matches!(mode.as_str(), "off" | "observe" | "active") {
                    return Err(SurfaceError::invalid(
                        "--mode requires off, observe, or active",
                    ));
                }
            }
            "--dry-run" => dry_run = true,
            "--restore" => restore = true,
            _ => {
                return Err(SurfaceError::invalid(format!(
                    "unexpected setup argument '{argument}'"
                )));
            }
        }
    }
    if dry_run && restore {
        return Err(SurfaceError::invalid(
            "--dry-run and --restore are mutually exclusive",
        ));
    }
    let options = Options {
        target,
        config_path: config_path.ok_or_else(|| SurfaceError::invalid("--config is required"))?,
        command,
        store_path,
        roots,
        mode,
        dry_run,
        restore,
    };
    validate_path(&options.config_path)?;
    let receipt = if options.restore {
        restore_backup(&options)?
    } else {
        install(&options)?
    };
    write_json_line(output, &receipt)
}

fn install(options: &Options) -> Result<Value, SurfaceError> {
    let command = options
        .command
        .as_ref()
        .ok_or_else(|| SurfaceError::invalid("--command is required for installation"))?;
    let original = read_existing(&options.config_path)?;
    let mut document = match original.as_deref() {
        Some(bytes) => serde_json::from_slice::<Value>(bytes)
            .map_err(|_| SurfaceError::invalid("configuration JSON is malformed"))?,
        None => json!({}),
    };
    if !document.is_object() {
        return Err(SurfaceError::invalid(
            "configuration root must be a JSON object",
        ));
    }
    let changed = match options.target {
        Target::Codex => install_codex(&mut document, options, command)?,
        Target::Claude => install_claude(&mut document, options, command)?,
    };
    let action = if !changed {
        "unchanged"
    } else if options.dry_run {
        "dry_run"
    } else {
        create_backup(&options.config_path, original.as_deref())?;
        let bytes = serde_json::to_vec_pretty(&document)
            .map_err(|_| SurfaceError::invalid("cannot serialize configuration"))?;
        atomic_write(&options.config_path, &bytes)?;
        "installed"
    };
    let managed_entry = match options.target {
        Target::Codex => document
            .pointer("/hooks/PostToolUse")
            .and_then(Value::as_array)
            .and_then(|groups| {
                groups.iter().find(|group| {
                    group
                        .pointer("/hooks/0/statusMessage")
                        .and_then(Value::as_str)
                        == Some(CODEX_SENTINEL)
                })
            })
            .cloned()
            .unwrap_or(Value::Null),
        Target::Claude => document
            .pointer("/mcpServers/distill")
            .cloned()
            .unwrap_or(Value::Null),
    };
    Ok(json!({
        "schema_version": SETUP_SCHEMA_VERSION,
        "target": target_name(options.target),
        "action": action,
        "config_path": options.config_path,
        "backup_path": backup_path(&options.config_path),
        "managed_entry": managed_entry,
    }))
}

fn install_codex(
    document: &mut Value,
    options: &Options,
    binary: &str,
) -> Result<bool, SurfaceError> {
    let root = document
        .as_object_mut()
        .ok_or_else(|| SurfaceError::invalid("configuration root must be an object"))?;
    let hooks = object_entry(root, "hooks")?;
    let groups = array_entry(hooks, "PostToolUse")?;
    let command = hook_command(binary, options);
    let desired = json!({
        "matcher": "*",
        "hooks": [{
            "type": "command",
            "command": command,
            "timeout": 30,
            "statusMessage": CODEX_SENTINEL,
        }]
    });
    if let Some(existing) = groups.iter_mut().find(|group| {
        group
            .pointer("/hooks/0/statusMessage")
            .and_then(Value::as_str)
            == Some(CODEX_SENTINEL)
    }) {
        if *existing == desired {
            return Ok(false);
        }
        *existing = desired;
        return Ok(true);
    }
    groups.push(desired);
    Ok(true)
}

fn install_claude(
    document: &mut Value,
    options: &Options,
    binary: &str,
) -> Result<bool, SurfaceError> {
    let root = document
        .as_object_mut()
        .ok_or_else(|| SurfaceError::invalid("configuration root must be an object"))?;
    let servers = object_entry(root, "mcpServers")?;
    let mut args = Vec::new();
    if let Some(path) = &options.store_path {
        args.push("--store".to_owned());
        args.push(path.to_string_lossy().into_owned());
    }
    for root in &options.roots {
        validate_root_spec(root)?;
        args.push("--root".to_owned());
        args.push(root.clone());
    }
    args.push("mcp".to_owned());
    let desired = json!({
        "command": binary,
        "args": args,
    });
    if servers.get("distill") == Some(&desired) {
        return Ok(false);
    }
    servers.insert("distill".to_owned(), desired);
    Ok(true)
}

fn hook_command(binary: &str, options: &Options) -> String {
    let mut parts = vec![binary.to_owned()];
    if let Some(path) = &options.store_path {
        parts.push("--store".to_owned());
        parts.push(path.to_string_lossy().into_owned());
    }
    parts.push("codex-hook".to_owned());
    parts.push("--mode".to_owned());
    parts.push(options.mode.clone());
    parts
        .iter()
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ")
}

fn restore_backup(options: &Options) -> Result<Value, SurfaceError> {
    let backup = backup_path(&options.config_path);
    let absent = absent_path(&options.config_path);
    let action = if backup.exists() {
        let bytes = read_existing(&backup)?
            .ok_or_else(|| SurfaceError::invalid("exact configuration backup disappeared"))?;
        atomic_write(&options.config_path, &bytes)?;
        "restored"
    } else if absent.exists() {
        if options.config_path.exists() {
            fs::remove_file(&options.config_path)
                .map_err(|_| SurfaceError::invalid("cannot restore absent configuration"))?;
        }
        "restored_absent"
    } else {
        return Err(SurfaceError::invalid(
            "no Distill configuration backup exists",
        ));
    };
    Ok(json!({
        "schema_version": SETUP_SCHEMA_VERSION,
        "target": target_name(options.target),
        "action": action,
        "config_path": options.config_path,
        "backup_path": backup,
    }))
}

fn create_backup(path: &Path, original: Option<&[u8]>) -> Result<(), SurfaceError> {
    let backup = backup_path(path);
    let absent = absent_path(path);
    if backup.exists() || absent.exists() {
        return Ok(());
    }
    match original {
        Some(bytes) => atomic_write(&backup, bytes),
        None => atomic_write(
            &absent,
            b"configuration did not exist before Distill setup\n",
        ),
    }
}

fn read_existing(path: &Path) -> Result<Option<Vec<u8>>, SurfaceError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(SurfaceError::invalid(
            "refusing to edit a symlinked configuration",
        )),
        Ok(metadata) if !metadata.is_file() => Err(SurfaceError::invalid(
            "configuration path is not a regular file",
        )),
        Ok(_) => fs::read(path)
            .map(Some)
            .map_err(|_| SurfaceError::invalid("cannot read configuration")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(SurfaceError::invalid("cannot inspect configuration")),
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), SurfaceError> {
    let parent = path
        .parent()
        .ok_or_else(|| SurfaceError::invalid("configuration path has no parent"))?;
    fs::create_dir_all(parent)
        .map_err(|_| SurfaceError::invalid("cannot create configuration directory"))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| SurfaceError::invalid("configuration filename is invalid"))?;
    let temporary = parent.join(format!(".{name}.distill-tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(|_| SurfaceError::invalid("cannot create atomic configuration file"))?;
    let write_result = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| SurfaceError::invalid("cannot write configuration atomically"));
    if let Err(error) = write_result {
        let _cleanup = fs::remove_file(&temporary);
        return Err(error);
    }
    set_private_mode(&temporary)?;
    fs::rename(&temporary, path).map_err(|_| {
        let _cleanup = fs::remove_file(&temporary);
        SurfaceError::invalid("cannot replace configuration atomically")
    })?;
    Ok(())
}

#[cfg(unix)]
fn set_private_mode(path: &Path) -> Result<(), SurfaceError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|_| SurfaceError::invalid("cannot enforce configuration permissions"))
}

#[cfg(not(unix))]
fn set_private_mode(_path: &Path) -> Result<(), SurfaceError> {
    Err(SurfaceError::invalid(
        "configuration permission enforcement is unsupported on this platform",
    ))
}

fn validate_path(path: &Path) -> Result<(), SurfaceError> {
    if !path.is_absolute()
        || !path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
    {
        return Err(SurfaceError::invalid(
            "--config must be a normalized absolute path",
        ));
    }
    Ok(())
}

fn validate_root_spec(root: &str) -> Result<(), SurfaceError> {
    let (id, path) = root
        .split_once('=')
        .ok_or_else(|| SurfaceError::invalid("--root requires ID=PATH"))?;
    if id.is_empty() || !Path::new(path).is_absolute() {
        return Err(SurfaceError::invalid(
            "--root requires a nonempty ID and absolute path",
        ));
    }
    Ok(())
}

fn object_entry<'a>(
    object: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Map<String, Value>, SurfaceError> {
    let value = object
        .entry(key.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    value
        .as_object_mut()
        .ok_or_else(|| SurfaceError::invalid(format!("{key} must be a JSON object")))
}

fn array_entry<'a>(
    object: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Vec<Value>, SurfaceError> {
    let value = object
        .entry(key.to_owned())
        .or_insert_with(|| Value::Array(Vec::new()));
    value
        .as_array_mut()
        .ok_or_else(|| SurfaceError::invalid(format!("{key} must be a JSON array")))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn backup_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{}.distill-backup",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config")
    ))
}

fn absent_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{}.distill-backup-absent",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config")
    ))
}

fn target_name(target: Target) -> &'static str {
    match target {
        Target::Codex => "codex",
        Target::Claude => "claude",
    }
}

fn take(args: &mut VecDeque<String>, flag: &str) -> Result<String, SurfaceError> {
    args.pop_front()
        .ok_or_else(|| SurfaceError::invalid(format!("{flag} requires a value")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    fn invoke(arguments: Vec<String>) -> Result<Value, SurfaceError> {
        let mut output = Vec::new();
        run(arguments.into(), &mut output)?;
        serde_json::from_slice(&output)
            .map_err(|_| SurfaceError::invalid("test output is not JSON"))
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
}
