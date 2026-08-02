use super::{ManagedEdit, document};
use crate::surface::SurfaceError;
use serde_json::{Value, json};
use std::path::Path;

pub(super) fn install(
    document: &mut Value,
    store_path: Option<&Path>,
    binary: &str,
    roots: &[String],
) -> Result<ManagedEdit, SurfaceError> {
    let root = document
        .as_object_mut()
        .ok_or_else(|| SurfaceError::invalid("configuration root must be an object"))?;
    let servers = document::object_entry(root, "mcpServers")?;
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
    let changed = servers.get("distill") != Some(&desired);
    if changed {
        servers.insert("distill".to_owned(), desired.clone());
    }
    Ok(ManagedEdit {
        changed,
        entry: desired,
    })
}
