use crate::{
    codex,
    surface::{SurfaceError, write_json_line},
};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeSet, VecDeque},
    fs::{self, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
};

const SETUP_SCHEMA_VERSION: &str = "distill.setup/v1";
const ABSENT_MARKER: &[u8] = b"configuration did not exist before Distill setup\n";

#[derive(Debug)]
enum TargetOptions {
    Codex { mode: codex::Mode },
    Claude { roots: Vec<String> },
}

impl TargetOptions {
    fn kind(&self) -> Target {
        match self {
            Self::Codex { .. } => Target::Codex,
            Self::Claude { .. } => Target::Claude,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Codex,
    Claude,
}

#[derive(Debug)]
enum SetupAction {
    Install { command: String, dry_run: bool },
    Restore,
}

#[derive(Debug)]
struct Options {
    target: TargetOptions,
    config_path: PathBuf,
    store_path: Option<PathBuf>,
    action: SetupAction,
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
    let mut mode = None;
    let mut dry_run = false;
    let mut restore = false;
    while let Some(argument) = args.pop_front() {
        match argument.as_str() {
            "--config" => config_path = Some(PathBuf::from(take(&mut args, "--config")?)),
            "--command" => command = Some(take(&mut args, "--command")?),
            "--store" => store_path = Some(PathBuf::from(take(&mut args, "--store")?)),
            "--root" => roots.push(take(&mut args, "--root")?),
            "--mode" => mode = Some(take(&mut args, "--mode")?),
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
    let config_path = config_path.ok_or_else(|| SurfaceError::invalid("--config is required"))?;
    let target = match target {
        Target::Codex => {
            if !roots.is_empty() {
                return Err(SurfaceError::invalid(
                    "--root does not apply to Codex setup",
                ));
            }
            let mode = match mode.as_deref() {
                Some(mode) => codex::Mode::parse(mode).ok_or_else(|| {
                    SurfaceError::invalid("--mode requires off, observe, or active")
                })?,
                None => codex::Mode::Active,
            };
            TargetOptions::Codex { mode }
        }
        Target::Claude => {
            if mode.is_some() {
                return Err(SurfaceError::invalid(
                    "--mode does not apply to Claude setup",
                ));
            }
            let mut ids = BTreeSet::new();
            for root in &roots {
                let id = validate_root_spec(root)?;
                if !ids.insert(id) {
                    return Err(SurfaceError::invalid(
                        "Claude setup root IDs must be unique",
                    ));
                }
            }
            TargetOptions::Claude { roots }
        }
    };
    validate_path(&config_path)?;
    let action = if restore {
        SetupAction::Restore
    } else {
        SetupAction::Install {
            command: command
                .ok_or_else(|| SurfaceError::invalid("--command is required for installation"))?,
            dry_run,
        }
    };
    let options = Options {
        target,
        config_path,
        store_path,
        action,
    };
    let receipt = match &options.action {
        SetupAction::Restore => restore_backup(&options)?,
        SetupAction::Install { command, dry_run } => install(&options, command, *dry_run)?,
    };
    write_json_line(output, &receipt)
}

fn install(options: &Options, command: &str, dry_run: bool) -> Result<Value, SurfaceError> {
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
    let changed = match &options.target {
        TargetOptions::Codex { mode } => {
            install_codex(&mut document, &options.store_path, command, *mode)?
        }
        TargetOptions::Claude { roots } => {
            install_claude(&mut document, &options.store_path, command, roots)?
        }
    };
    let action = if !changed {
        "unchanged"
    } else if dry_run {
        "dry_run"
    } else {
        create_backup(&options.config_path, original.as_deref())?;
        let bytes = serde_json::to_vec_pretty(&document)
            .map_err(|_| SurfaceError::invalid("cannot serialize configuration"))?;
        atomic_write(&options.config_path, &bytes)?;
        "installed"
    };
    let managed_entry = match options.target.kind() {
        Target::Codex => document
            .pointer("/hooks/PostToolUse")
            .and_then(Value::as_array)
            .and_then(|groups| groups.iter().find(|group| is_managed_codex_group(group)))
            .cloned()
            .unwrap_or(Value::Null),
        Target::Claude => document
            .pointer("/mcpServers/distill")
            .cloned()
            .unwrap_or(Value::Null),
    };
    Ok(json!({
        "schema_version": SETUP_SCHEMA_VERSION,
        "target": target_name(options.target.kind()),
        "action": action,
        "config_path": options.config_path,
        "backup_path": backup_path(&options.config_path),
        "managed_entry": managed_entry,
    }))
}

fn install_codex(
    document: &mut Value,
    store_path: &Option<PathBuf>,
    binary: &str,
    mode: codex::Mode,
) -> Result<bool, SurfaceError> {
    let root = document
        .as_object_mut()
        .ok_or_else(|| SurfaceError::invalid("configuration root must be an object"))?;
    let hooks = object_entry(root, "hooks")?;
    let groups = array_entry(hooks, "PostToolUse")?;
    let command = hook_command(binary, store_path.as_deref(), mode);
    if groups
        .iter()
        .any(|group| has_codex_status(group) && !is_managed_codex_group(group))
    {
        return Err(SurfaceError::invalid(
            "Codex configuration contains an ambiguous Distill status message",
        ));
    }
    let managed_count = groups
        .iter()
        .filter(|group| is_managed_codex_group(group))
        .count();
    if managed_count > 1 {
        return Err(SurfaceError::invalid(
            "Codex configuration contains duplicate Distill hook entries",
        ));
    }
    let desired = json!({
        "matcher": codex::SETUP_MATCHER,
        "hooks": [{
            "type": "command",
            "command": command,
            "timeout": codex::SETUP_TIMEOUT_SECONDS,
            "statusMessage": codex::SETUP_STATUS_MESSAGE,
        }]
    });
    if let Some(existing) = groups
        .iter_mut()
        .find(|group| is_managed_codex_group(group))
    {
        if *existing == desired {
            return Ok(false);
        }
        *existing = desired;
        return Ok(true);
    }
    groups.push(desired);
    Ok(true)
}

fn has_codex_status(group: &Value) -> bool {
    group
        .pointer("/hooks/0/statusMessage")
        .and_then(Value::as_str)
        == Some(codex::SETUP_STATUS_MESSAGE)
}

fn is_managed_codex_group(group: &Value) -> bool {
    let Some(group) = group.as_object() else {
        return false;
    };
    if group.len() != 2 {
        return false;
    }
    let Some(hooks) = group.get("hooks").and_then(Value::as_array) else {
        return false;
    };
    let [hook] = hooks.as_slice() else {
        return false;
    };
    let Some(hook) = hook.as_object() else {
        return false;
    };
    if hook.len() != 4 {
        return false;
    }
    group.get("matcher").and_then(Value::as_str) == Some(codex::SETUP_MATCHER)
        && hook.get("type").and_then(Value::as_str) == Some("command")
        && hook.get("timeout").and_then(Value::as_u64) == Some(codex::SETUP_TIMEOUT_SECONDS)
        && hook.get("statusMessage").and_then(Value::as_str) == Some(codex::SETUP_STATUS_MESSAGE)
        && hook
            .get("command")
            .and_then(Value::as_str)
            .and_then(parse_shell_command)
            .is_some_and(|arguments| match arguments.as_slice() {
                [_, hook, mode_flag, mode] => {
                    hook == "codex-hook"
                        && mode_flag == "--mode"
                        && codex::Mode::parse(mode).is_some()
                }
                [_, store_flag, _, hook, mode_flag, mode] => {
                    store_flag == "--store"
                        && hook == "codex-hook"
                        && mode_flag == "--mode"
                        && codex::Mode::parse(mode).is_some()
                }
                _ => false,
            })
}

fn parse_shell_command(command: &str) -> Option<Vec<String>> {
    let mut remaining = command;
    let mut arguments = Vec::new();
    while !remaining.is_empty() {
        remaining = remaining.strip_prefix('\'')?;
        let mut argument = String::new();
        loop {
            let quote = remaining.find('\'')?;
            argument.push_str(&remaining[..quote]);
            remaining = &remaining[quote..];
            if let Some(rest) = remaining.strip_prefix("'\"'\"'") {
                argument.push('\'');
                remaining = rest;
                continue;
            }
            remaining = remaining.strip_prefix('\'')?;
            break;
        }
        arguments.push(argument);
        if remaining.is_empty() {
            break;
        }
        remaining = remaining.strip_prefix(' ')?;
        if remaining.is_empty() {
            return None;
        }
    }
    (arguments
        .iter()
        .map(|argument| shell_quote(argument))
        .collect::<Vec<_>>()
        .join(" ")
        == command)
        .then_some(arguments)
}

fn install_claude(
    document: &mut Value,
    store_path: &Option<PathBuf>,
    binary: &str,
    roots: &[String],
) -> Result<bool, SurfaceError> {
    let root = document
        .as_object_mut()
        .ok_or_else(|| SurfaceError::invalid("configuration root must be an object"))?;
    let servers = object_entry(root, "mcpServers")?;
    let mut args = Vec::new();
    if let Some(path) = store_path {
        args.push("--store".to_owned());
        args.push(path.to_string_lossy().into_owned());
    }
    for root in roots {
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

fn hook_command(binary: &str, store_path: Option<&Path>, mode: codex::Mode) -> String {
    let mut parts = vec![binary.to_owned()];
    if let Some(path) = store_path {
        parts.push("--store".to_owned());
        parts.push(path.to_string_lossy().into_owned());
    }
    parts.extend(codex::setup_hook_arguments(mode));
    parts
        .iter()
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ")
}

fn restore_backup(options: &Options) -> Result<Value, SurfaceError> {
    let current = read_existing(&options.config_path)?;
    let backup = backup_path(&options.config_path);
    let absent = absent_path(&options.config_path);
    let backup_bytes = read_existing(&backup)?;
    let absent_bytes = read_existing(&absent)?;
    let action = match (backup_bytes, absent_bytes) {
        (Some(bytes), None) => {
            atomic_write(&options.config_path, &bytes)?;
            "restored"
        }
        (None, Some(marker)) if marker == ABSENT_MARKER => {
            if current.is_some() {
                fs::remove_file(&options.config_path)
                    .map_err(|_| SurfaceError::invalid("cannot restore absent configuration"))?;
            }
            "restored_absent"
        }
        (None, None) => {
            return Err(SurfaceError::invalid(
                "no Distill configuration backup exists",
            ));
        }
        _ => {
            return Err(SurfaceError::invalid(
                "Distill configuration backup state is ambiguous",
            ));
        }
    };
    Ok(json!({
        "schema_version": SETUP_SCHEMA_VERSION,
        "target": target_name(options.target.kind()),
        "action": action,
        "config_path": options.config_path,
        "backup_path": backup,
    }))
}

fn create_backup(path: &Path, original: Option<&[u8]>) -> Result<(), SurfaceError> {
    let backup = backup_path(path);
    let absent = absent_path(path);
    let backup_bytes = read_existing(&backup)?;
    let absent_bytes = read_existing(&absent)?;
    match (original, backup_bytes, absent_bytes) {
        (Some(_), Some(_), None) => Ok(()),
        (Some(_), None, Some(marker)) if marker == ABSENT_MARKER => Ok(()),
        (None, None, Some(marker)) if marker == ABSENT_MARKER => Ok(()),
        (Some(bytes), None, None) => atomic_write(&backup, bytes),
        (None, None, None) => atomic_write(&absent, ABSENT_MARKER),
        _ => Err(SurfaceError::invalid(
            "Distill configuration backup state is ambiguous",
        )),
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
    let mut file = open_private_temporary(&temporary)?;
    let write_result = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| SurfaceError::invalid("cannot write configuration atomically"));
    if let Err(error) = write_result {
        let _cleanup = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = set_private_mode(&temporary) {
        let _cleanup = fs::remove_file(&temporary);
        return Err(error);
    }
    fs::rename(&temporary, path).map_err(|_| {
        let _cleanup = fs::remove_file(&temporary);
        SurfaceError::invalid("cannot replace configuration atomically")
    })?;
    Ok(())
}

#[cfg(unix)]
fn open_private_temporary(path: &Path) -> Result<fs::File, SurfaceError> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| SurfaceError::invalid("cannot create atomic configuration file"))
}

#[cfg(not(unix))]
fn open_private_temporary(_path: &Path) -> Result<fs::File, SurfaceError> {
    Err(SurfaceError::invalid(
        "configuration permission enforcement is unsupported on this platform",
    ))
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

fn validate_root_spec(root: &str) -> Result<&str, SurfaceError> {
    let (id, path) = root
        .split_once('=')
        .ok_or_else(|| SurfaceError::invalid("--root requires ID=PATH"))?;
    if id.is_empty() || !Path::new(path).is_absolute() {
        return Err(SurfaceError::invalid(
            "--root requires a nonempty ID and absolute path",
        ));
    }
    Ok(id)
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
#[path = "setup/tests.rs"]
mod tests;
