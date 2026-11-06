//! The JSON Schema subset ShiShi's prompts and tools use: type, enum, const,
//! properties, required, additionalProperties, items and the length, item,
//! property and numeric bounds. Errors name the path and the rule, never
//! the value.

use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("schema at {path}: {constraint}")]
pub struct ValidationError {
    path: String,
    constraint: String,
}

impl ValidationError {
    pub fn new(path: impl Into<String>, constraint: impl Into<String>) -> Self {
        Self { path: path.into(), constraint: constraint.into() }
    }
}

pub fn validate(schema: &Value, instance: &Value) -> Result<(), ValidationError> {
    validate_at(schema, instance, "$")
}

/// Fill explicit scalar `default`s where a value is missing or invalid,
/// then validate strictly. Tool arguments use [`validate`] instead.
pub fn validate_output(schema: &Value, mut value: Value) -> Result<Value, ValidationError> {
    defaults(schema, &mut value);
    validate(schema, &value)?;
    Ok(value)
}

fn scalar_default(schema: &Value) -> Option<&Value> {
    schema.get("default").filter(|v| !v.is_array() && !v.is_object() && validate(schema, v).is_ok())
}

fn defaults(schema: &Value, value: &mut Value) {
    if let Some(default) = scalar_default(schema) {
        if validate(schema, value).is_err() {
            *value = default.clone();
            return;
        }
    }
    if let (Some(properties), Some(object)) = (schema["properties"].as_object(), value.as_object_mut()) {
        for (name, child) in properties {
            match object.get_mut(name) {
                Some(value) => defaults(child, value),
                None => {
                    if let Some(default) = scalar_default(child) {
                        object.insert(name.clone(), default.clone());
                    }
                }
            }
        }
    } else if let (Some(items), Some(array)) = (schema.get("items"), value.as_array_mut()) {
        array.iter_mut().for_each(|v| defaults(items, v));
    }
}

fn bounds(schema: &Value, min: &str, max: &str, n: f64, path: &str) -> Result<(), ValidationError> {
    for (key, lower) in [(min, true), (max, false)] {
        if schema[key].as_f64().is_some_and(|b| if lower { n < b } else { n > b }) {
            return Err(ValidationError::new(path, format!("{key} constraint")));
        }
    }
    Ok(())
}

fn matches_type(kind: &str, v: &Value) -> bool {
    match kind {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "number" => v.is_number(),
        "integer" => v.is_i64() || v.is_u64(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        _ => false,
    }
}

fn validate_at(schema: &Value, instance: &Value, path: &str) -> Result<(), ValidationError> {
    let fail = |rule: &str| Err(ValidationError::new(path, rule));
    match schema {
        Value::Bool(true) => return Ok(()),
        Value::Bool(false) => return fail("schema is false"),
        _ => {}
    }
    if let Some(types) = schema.get("type") {
        let names: Vec<&str> = types
            .as_str()
            .into_iter()
            .chain(types.as_array().into_iter().flatten().filter_map(Value::as_str))
            .collect();
        if !names.iter().any(|kind| matches_type(kind, instance)) {
            return fail(&format!("type must be one of: {}", names.join(", ")));
        }
    }
    if schema["enum"].as_array().is_some_and(|allowed| !allowed.contains(instance)) {
        return fail("value is not in enum");
    }
    if schema.get("const").is_some_and(|c| c != instance) {
        return fail("value does not equal const");
    }
    match instance {
        Value::Object(values) => {
            bounds(schema, "minProperties", "maxProperties", values.len() as f64, path)?;
            for name in schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                if !values.contains_key(name) {
                    return Err(ValidationError::new(format!("{path}.{name}"), "required property is missing"));
                }
            }
            let properties = schema["properties"].as_object();
            for (name, child) in values {
                match properties.and_then(|p| p.get(name)) {
                    Some(child_schema) => validate_at(child_schema, child, &format!("{path}.{name}"))?,
                    None => match schema.get("additionalProperties") {
                        Some(Value::Bool(false)) => {
                            return fail("additionalProperties: unexpected property is not allowed")
                        }
                        Some(extra @ Value::Object(_)) => validate_at(extra, child, &format!("{path}.*"))?,
                        _ => {}
                    },
                }
            }
        }
        Value::Array(items) => {
            bounds(schema, "minItems", "maxItems", items.len() as f64, path)?;
            if let Some(item) = schema.get("items") {
                for (i, child) in items.iter().enumerate() {
                    validate_at(item, child, &format!("{path}[{i}]"))?;
                }
            }
        }
        Value::String(text) => bounds(schema, "minLength", "maxLength", text.chars().count() as f64, path)?,
        Value::Number(n) => bounds(schema, "minimum", "maximum", n.as_f64().unwrap(), path)?,
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests;
