use crate::surface::SurfaceError;
use serde_json::{Map, Value};

pub(super) fn object_entry<'a>(
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

pub(super) fn array_entry<'a>(
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
