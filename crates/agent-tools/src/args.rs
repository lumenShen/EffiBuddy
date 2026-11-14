//! Argument helpers shared by tools.
//!
//! Model-produced JSON is untrusted input: every tool validates shape and
//! range before touching the filesystem or spawning a process.

use serde_json::{Map, Value};

use effibuddy_agent_core::ToolError;

pub fn as_object(arguments: &Value) -> Result<&Map<String, Value>, ToolError> {
    arguments
        .as_object()
        .ok_or_else(|| ToolError::new("tool arguments must be a JSON object"))
}

pub fn required_string(arguments: &Value, key: &str) -> Result<String, ToolError> {
    let object = as_object(arguments)?;
    match object.get(key) {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(ToolError::new(format!("`{key}` must be a string"))),
        None => Err(ToolError::new(format!("missing required argument `{key}`"))),
    }
}

pub fn optional_string(arguments: &Value, key: &str) -> Result<Option<String>, ToolError> {
    let object = as_object(arguments)?;
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(ToolError::new(format!("`{key}` must be a string"))),
    }
}

pub fn optional_u64(arguments: &Value, key: &str) -> Result<Option<u64>, ToolError> {
    let object = as_object(arguments)?;
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(value)) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| ToolError::new(format!("`{key}` must be a non-negative integer"))),
        Some(_) => Err(ToolError::new(format!(
            "`{key}` must be a non-negative integer"
        ))),
    }
}

pub fn optional_bool(arguments: &Value, key: &str) -> Result<Option<bool>, ToolError> {
    let object = as_object(arguments)?;
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(ToolError::new(format!("`{key}` must be a boolean"))),
    }
}
