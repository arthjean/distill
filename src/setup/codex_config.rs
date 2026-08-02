use super::{ManagedEdit, document};
use crate::{codex, surface::SurfaceError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HookGroup {
    matcher: String,
    hooks: Vec<CommandHook>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CommandHook {
    #[serde(rename = "type")]
    kind: String,
    command: String,
    timeout: u64,
    #[serde(rename = "statusMessage")]
    status_message: String,
}

pub(super) fn install(
    document: &mut Value,
    store_path: Option<&Path>,
    binary: &str,
    mode: codex::Mode,
) -> Result<ManagedEdit, SurfaceError> {
    let root = document
        .as_object_mut()
        .ok_or_else(|| SurfaceError::invalid("configuration root must be an object"))?;
    let hooks = document::object_entry(root, "hooks")?;
    let groups = document::array_entry(hooks, "PostToolUse")?;
    if groups
        .iter()
        .any(|group| has_status(group) && !is_managed(group))
    {
        return Err(SurfaceError::invalid(
            "Codex configuration contains an ambiguous Distill status message",
        ));
    }
    if groups.iter().filter(|group| is_managed(group)).count() > 1 {
        return Err(SurfaceError::invalid(
            "Codex configuration contains duplicate Distill hook entries",
        ));
    }

    let desired = serde_json::to_value(HookGroup {
        matcher: codex::SETUP_MATCHER.to_owned(),
        hooks: vec![CommandHook {
            kind: "command".to_owned(),
            command: hook_command(binary, store_path, mode),
            timeout: codex::SETUP_TIMEOUT_SECONDS,
            status_message: codex::SETUP_STATUS_MESSAGE.to_owned(),
        }],
    })
    .map_err(|_| SurfaceError::invalid("cannot serialize Codex hook configuration"))?;
    if let Some(existing) = groups.iter_mut().find(|group| is_managed(group)) {
        if *existing == desired {
            return Ok(ManagedEdit {
                changed: false,
                entry: desired,
            });
        }
        *existing = desired.clone();
        return Ok(ManagedEdit {
            changed: true,
            entry: desired,
        });
    }
    groups.push(desired.clone());
    Ok(ManagedEdit {
        changed: true,
        entry: desired,
    })
}

fn has_status(group: &Value) -> bool {
    group
        .pointer("/hooks/0/statusMessage")
        .and_then(Value::as_str)
        == Some(codex::SETUP_STATUS_MESSAGE)
}

fn is_managed(group: &Value) -> bool {
    let Ok(group) = serde_json::from_value::<HookGroup>(group.clone()) else {
        return false;
    };
    let [hook] = group.hooks.as_slice() else {
        return false;
    };
    group.matcher == codex::SETUP_MATCHER
        && hook.kind == "command"
        && hook.timeout == codex::SETUP_TIMEOUT_SECONDS
        && hook.status_message == codex::SETUP_STATUS_MESSAGE
        && parse_shell_command(&hook.command).is_some_and(|arguments| match arguments.as_slice() {
            [_, hook, mode_flag, mode] => {
                hook == "codex-hook" && mode_flag == "--mode" && codex::Mode::parse(mode).is_some()
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

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}
