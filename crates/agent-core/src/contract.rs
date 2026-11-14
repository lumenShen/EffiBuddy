//! JSON extraction and a practical JSON Schema validator.

use serde_json::Value;

// ---------------------------------------------------------------------------
// JSON contract helpers
// ---------------------------------------------------------------------------

/// Extract a JSON value from model output. Accepts bare JSON, fenced JSON and
/// a leading/trailing prose wrapper around one object or array.
pub fn extract_json_value(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str(trimmed) {
        return Some(value);
    }
    if let Some(stripped) = strip_code_fence(trimmed) {
        if let Ok(value) = serde_json::from_str(stripped.trim()) {
            return Some(value);
        }
    }
    let start_object = trimmed.find('{');
    let start_array = trimmed.find('[');
    let start = match (start_object, start_array) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }?;
    let closing = if trimmed.as_bytes()[start] == b'{' {
        '}'
    } else {
        ']'
    };
    let end = trimmed.rfind(closing)?;
    if end <= start {
        return None;
    }
    serde_json::from_str(&trimmed[start..=end]).ok()
}

fn strip_code_fence(text: &str) -> Option<&str> {
    let stripped = text.strip_prefix("```")?;
    let newline = stripped.find('\n')?;
    let body = &stripped[newline + 1..];
    let end = body.rfind("```")?;
    Some(&body[..end])
}

/// Validate `value` against the practical JSON Schema subset documented on
/// this module. Error strings include the failing path.
pub fn validate_json(value: &Value, schema: &Value) -> Result<(), String> {
    validate_at(value, schema, "$", 0)
}

fn validate_at(value: &Value, schema: &Value, path: &str, depth: usize) -> Result<(), String> {
    if depth > 64 {
        return Err(format!("{path}: schema nesting is too deep"));
    }
    match schema {
        Value::Bool(true) => return Ok(()),
        Value::Bool(false) => return Err(format!("{path}: schema forbids any value")),
        Value::Object(_) => {}
        _ => return Err(format!("{path}: schema must be an object or boolean")),
    }
    let object = schema.as_object().expect("checked object");

    if let Some(const_value) = object.get("const") {
        if value != const_value {
            return Err(format!("{path}: expected constant {const_value}"));
        }
    }
    if let Some(options) = object.get("enum").and_then(Value::as_array) {
        if !options.iter().any(|option| option == value) {
            return Err(format!(
                "{path}: value is not one of the allowed enum values"
            ));
        }
    }
    if let Some(types) = object.get("type") {
        let matched = match types {
            Value::String(single) => type_matches(value, single),
            Value::Array(entries) => entries
                .iter()
                .filter_map(Value::as_str)
                .any(|single| type_matches(value, single)),
            _ => true,
        };
        if !matched {
            return Err(format!(
                "{path}: expected type {}, got {}",
                compact_json(types),
                value_kind(value)
            ));
        }
    }

    if let Some(all_of) = object.get("allOf").and_then(Value::as_array) {
        for (index, branch) in all_of.iter().enumerate() {
            validate_at(value, branch, &format!("{path}.allOf[{index}]"), depth + 1)?;
        }
    }
    if let Some(any_of) = object.get("anyOf").and_then(Value::as_array) {
        if !any_of
            .iter()
            .any(|branch| validate_at(value, branch, path, depth + 1).is_ok())
        {
            return Err(format!("{path}: value does not match any anyOf branch"));
        }
    }
    if let Some(one_of) = object.get("oneOf").and_then(Value::as_array) {
        let matches = one_of
            .iter()
            .filter(|branch| validate_at(value, branch, path, depth + 1).is_ok())
            .count();
        if matches != 1 {
            return Err(format!(
                "{path}: expected exactly one oneOf branch to match, matched {matches}"
            ));
        }
    }
    if let Some(not) = object.get("not") {
        if validate_at(value, not, path, depth + 1).is_ok() {
            return Err(format!("{path}: value matches a forbidden schema"));
        }
    }

    if let Some(map) = value.as_object() {
        if let Some(required) = object.get("required").and_then(Value::as_array) {
            for key in required.iter().filter_map(Value::as_str) {
                if !map.contains_key(key) {
                    return Err(format!("{path}: missing required property `{key}`"));
                }
            }
        }
        if let Some(properties) = object.get("properties").and_then(Value::as_object) {
            for (key, child_schema) in properties {
                if let Some(child) = map.get(key) {
                    validate_at(child, child_schema, &format!("{path}.{key}"), depth + 1)?;
                }
            }
        }
        if object.get("additionalProperties") == Some(&Value::Bool(false)) {
            if let Some(properties) = object.get("properties").and_then(Value::as_object) {
                for key in map.keys() {
                    if !properties.contains_key(key) {
                        return Err(format!("{path}: unexpected property `{key}`"));
                    }
                }
            }
        }
        if let Some(min) = object.get("minProperties").and_then(Value::as_u64) {
            if (map.len() as u64) < min {
                return Err(format!("{path}: expected at least {min} properties"));
            }
        }
        if let Some(max) = object.get("maxProperties").and_then(Value::as_u64) {
            if (map.len() as u64) > max {
                return Err(format!("{path}: expected at most {max} properties"));
            }
        }
    }

    if let Some(items) = value.as_array() {
        if let Some(item_schema) = object.get("items") {
            for (index, child) in items.iter().enumerate() {
                validate_at(child, item_schema, &format!("{path}[{index}]"), depth + 1)?;
            }
        }
        if let Some(min) = object.get("minItems").and_then(Value::as_u64) {
            if (items.len() as u64) < min {
                return Err(format!("{path}: expected at least {min} items"));
            }
        }
        if let Some(max) = object.get("maxItems").and_then(Value::as_u64) {
            if (items.len() as u64) > max {
                return Err(format!("{path}: expected at most {max} items"));
            }
        }
    }

    if let Some(text) = value.as_str() {
        let length = text.chars().count() as u64;
        if let Some(min) = object.get("minLength").and_then(Value::as_u64) {
            if length < min {
                return Err(format!("{path}: string is shorter than {min} characters"));
            }
        }
        if let Some(max) = object.get("maxLength").and_then(Value::as_u64) {
            if length > max {
                return Err(format!("{path}: string is longer than {max} characters"));
            }
        }
    }

    if value.is_number() {
        let number = value.as_f64().unwrap_or(0.0);
        if let Some(min) = object.get("minimum").and_then(Value::as_f64) {
            if number < min {
                return Err(format!("{path}: number is below the minimum {min}"));
            }
        }
        if let Some(max) = object.get("maximum").and_then(Value::as_f64) {
            if number > max {
                return Err(format!("{path}: number is above the maximum {max}"));
            }
        }
        if let Some(min) = object.get("exclusiveMinimum").and_then(Value::as_f64) {
            if number <= min {
                return Err(format!("{path}: number must be greater than {min}"));
            }
        }
        if let Some(max) = object.get("exclusiveMaximum").and_then(Value::as_f64) {
            if number >= max {
                return Err(format!("{path}: number must be less than {max}"));
            }
        }
    }

    Ok(())
}

fn type_matches(value: &Value, type_name: &str) -> bool {
    match type_name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value
            .as_f64()
            .map(|number| number.fract() == 0.0)
            .unwrap_or(false),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.as_f64().is_some_and(|value| value.fract() == 0.0) => {
            "integer"
        }
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}
