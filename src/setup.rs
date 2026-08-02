use crate::{
    codex,
    surface::{SurfaceError, write_json_line},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, VecDeque},
    io::Write,
    path::{Component, Path, PathBuf},
};

mod backup;
mod claude;
mod codex_config;
mod document;

#[cfg(test)]
use backup::{ABSENT_MARKER, absent_path, backup_path};

const SETUP_SCHEMA_VERSION: &str = "distill.setup/v1";

#[derive(Debug)]
enum TargetOptions {
    Codex { mode: codex::Mode },
    Claude { roots: Vec<String> },
}

impl TargetOptions {
    fn name(&self) -> &'static str {
        match self {
            Self::Codex { .. } => "codex",
            Self::Claude { .. } => "claude",
        }
    }

    fn install(
        &self,
        document: &mut Value,
        store_path: Option<&Path>,
        command: &str,
    ) -> Result<ManagedEdit, SurfaceError> {
        match self {
            Self::Codex { mode } => codex_config::install(document, store_path, command, *mode),
            Self::Claude { roots } => claude::install(document, store_path, command, roots),
        }
    }
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

pub(super) struct ManagedEdit {
    pub changed: bool,
    pub entry: Value,
}

pub(crate) fn run<W: Write>(
    mut args: VecDeque<String>,
    output: &mut W,
) -> Result<(), SurfaceError> {
    let target_name = args
        .pop_front()
        .ok_or_else(|| SurfaceError::invalid("setup requires codex or claude"))?;

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
    let target = match target_name.as_str() {
        "codex" => {
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
        "claude" => {
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
        _ => return Err(SurfaceError::invalid("setup requires codex or claude")),
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
    let original = backup::read_existing(&options.config_path)?;
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
    let edit = options
        .target
        .install(&mut document, options.store_path.as_deref(), command)?;
    let action = if !edit.changed {
        "unchanged"
    } else if dry_run {
        "dry_run"
    } else {
        backup::create(&options.config_path, original.as_deref())?;
        let bytes = serde_json::to_vec_pretty(&document)
            .map_err(|_| SurfaceError::invalid("cannot serialize configuration"))?;
        backup::write(&options.config_path, &bytes)?;
        "installed"
    };
    Ok(json!({
        "schema_version": SETUP_SCHEMA_VERSION,
        "target": options.target.name(),
        "action": action,
        "config_path": options.config_path,
        "backup_path": backup::backup_path(&options.config_path),
        "managed_entry": edit.entry,
    }))
}

fn restore_backup(options: &Options) -> Result<Value, SurfaceError> {
    let restored = backup::restore(&options.config_path)?;
    Ok(json!({
        "schema_version": SETUP_SCHEMA_VERSION,
        "target": options.target.name(),
        "action": restored.action,
        "config_path": options.config_path,
        "backup_path": restored.backup_path,
    }))
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

fn take(args: &mut VecDeque<String>, flag: &str) -> Result<String, SurfaceError> {
    args.pop_front()
        .ok_or_else(|| SurfaceError::invalid(format!("{flag} requires a value")))
}

#[cfg(test)]
#[path = "setup/tests.rs"]
mod tests;
