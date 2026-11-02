//! Bounded JSON Schema subset used to validate provider output and tool
//! arguments locally.
//!
//! Scope is the subset actually used by `src/ai.rs`: typed objects, `properties`,
//! `required`, `additionalProperties` (boolean or schema), arrays with a single
//! `items` schema, `enum`, `const`, scalar types, `minLength`/`maxLength`,
//! `minItems`/`maxItems`, `minProperties`/`maxProperties`, and
//! `minimum`/`maximum`. Annotation keywords are accepted and ignored.
//!
//! Anything else fails closed during [`check_schema`] instead of being silently
//! skipped. Validation errors expose a bounded schema path and the failed
//! constraint, never the input value. Depth, schema-node, instance-node, string,
//! array and object limits prevent unbounded recursion or traversal.

use serde_json::Value;

pub const MAX_SCHEMA_DEPTH: usize = 32;
pub const MAX_SCHEMA_NODES: usize = 20_000;
pub const MAX_INSTANCE_DEPTH: usize = 64;
pub const MAX_INSTANCE_NODES: usize = 200_000;
pub const MAX_STRING_CHARS: usize = 4 * 1024 * 1024;
pub const MAX_ARRAY_ITEMS: usize = 100_000;
pub const MAX_OBJECT_PROPERTIES: usize = 10_000;
const MAX_PROPERTY_NAME_CHARS: usize = 128;
const MAX_ENUM_VALUES: usize = 1_024;
const MAX_ENUM_BYTES: usize = 64 * 1024;
const MAX_CONST_BYTES: usize = 64 * 1024;
const MAX_NUMERIC_BOUND: u64 = 100_000_000;
const MAX_PATH_CHARS: usize = 512;

/// A schema validation failure. `Display` contains only the schema path and the
/// failed constraint; input values are never formatted into it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("schema at {path}: {constraint}")]
pub struct ValidationError {
    path: String,
    constraint: String,
}

impl ValidationError {
    /// Host validation must describe the violated rule, never include input values.
    pub fn new(path: impl Into<String>, constraint: impl Into<String>) -> Self {
        Self {
            path: truncate_path(path.into()),
            constraint: truncate_text(constraint.into(), MAX_PATH_CHARS),
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn constraint(&self) -> &str {
        &self.constraint
    }
}

/// Validate a schema against the supported subset before using it for any
/// request. Unsupported validation keywords fail closed.
pub fn check_schema(schema: &Value) -> Result<(), ValidationError> {
    let mut nodes = 0usize;
    check_schema_at(schema, "$", 0, &mut nodes)
}

/// Check the schema and then validate one instance.
pub fn validate(schema: &Value, instance: &Value) -> Result<(), ValidationError> {
    CheckedSchema::new(schema)?.validate(instance)
}

fn validate_checked(schema: &Value, instance: &Value) -> Result<(), ValidationError> {
    validate_at(schema, instance, "$", 0, &mut 0)
}

/// Accept output with explicit scalar defaults, then validate it strictly.
/// Defaults are opt-in presentation fallbacks, never inferred from a type.
/// Tool arguments continue to use `validate` and cannot use this repair path.
pub fn validate_output(schema: &Value, value: Value) -> Result<Value, ValidationError> {
    CheckedSchema::new(schema)?.output(value)
}

/// A borrowed, immutable schema checked once for an entire model/tool loop.
pub(crate) struct CheckedSchema<'a>(&'a Value);

impl<'a> CheckedSchema<'a> {
    pub(crate) fn new(schema: &'a Value) -> Result<Self, ValidationError> {
        check_schema(schema)?;
        Ok(Self(schema))
    }

    pub(crate) fn validate(&self, instance: &Value) -> Result<(), ValidationError> {
        validate_checked(self.0, instance)
    }

    pub(crate) fn output(&self, mut value: Value) -> Result<Value, ValidationError> {
        output_defaults(self.0, &mut value, 0, &mut 0)?;
        self.validate(&value)?;
        Ok(value)
    }
}

fn scalar_default(schema: &Value) -> Option<&Value> {
    schema.get("default").filter(|value| {
        !value.is_array() && !value.is_object() && validate_checked(schema, value).is_ok()
    })
}

fn output_defaults(
    schema: &Value,
    value: &mut Value,
    depth: usize,
    nodes: &mut usize,
) -> Result<(), ValidationError> {
    *nodes += 1;
    if depth > MAX_INSTANCE_DEPTH || *nodes > MAX_INSTANCE_NODES {
        return Err(ValidationError::new("$", "output traversal limit exceeded"));
    }
    if let Some(default) = scalar_default(schema) {
        if validate_checked(schema, value).is_err() {
            *value = default.clone();
            return Ok(());
        }
    }
    if let (Some(properties), Some(object)) =
        (schema["properties"].as_object(), value.as_object_mut())
    {
        for (name, child) in properties {
            if let Some(value) = object.get_mut(name) {
                output_defaults(child, value, depth + 1, nodes)?;
            } else if let Some(default) = scalar_default(child) {
                object.insert(name.clone(), default.clone());
            }
        }
    } else if let (Some(items), Some(array)) = (schema.get("items"), value.as_array_mut()) {
        for value in array {
            output_defaults(items, value, depth + 1, nodes)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod output_default_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn optional_presentation_failure_does_not_relax_business_or_tool_validation() {
        let schema = json!({"type":"object","additionalProperties":false,"properties":{
            "actions":{"type":"array","items":{"type":"object","additionalProperties":false,
                "properties":{"status":{"type":"string","enum":["pending","done"]},
                    "summary":{"type":"string","maxLength":3,"default":""}},"required":["status","summary"]}}
        },"required":["actions"]});
        for summary in [
            None,
            Some(json!(null)),
            Some(json!(["bad"])),
            Some(json!("long")),
        ] {
            let mut value = json!({"actions":[{"status":"pending"}]});
            if let Some(summary) = summary {
                value["actions"][0]["summary"] = summary;
            }
            assert!(
                validate(&schema, &value).is_err(),
                "tool arguments stay strict"
            );
            assert_eq!(
                validate_output(&schema, value).unwrap()["actions"][0]["summary"],
                ""
            );
        }
        assert_eq!(
            validate_output(
                &schema,
                json!({"actions":[{"status":"done","summary":"ok"}]})
            )
            .unwrap()["actions"][0]["summary"],
            "ok"
        );
        for action in [
            json!({}),
            json!({"status":"invalid"}),
            json!({"status":"done","extra":true}),
        ] {
            assert!(validate_output(&schema, json!({"actions":[action]})).is_err());
        }
    }
}

fn check_schema_at(
    schema: &Value,
    path: &str,
    depth: usize,
    nodes: &mut usize,
) -> Result<(), ValidationError> {
    *nodes += 1;
    if *nodes > MAX_SCHEMA_NODES {
        return Err(ValidationError::new(path, "schema node limit exceeded"));
    }
    if depth > MAX_SCHEMA_DEPTH {
        return Err(ValidationError::new(path, "schema depth limit exceeded"));
    }
    let object = match schema {
        Value::Bool(_) => return Ok(()),
        Value::Object(object) => object,
        _ => {
            return Err(ValidationError::new(
                path,
                "schema must be an object or boolean",
            ))
        }
    };

    let mut types: Option<&[Value]> = None;

    for (keyword, value) in object {
        match keyword.as_str() {
            // Annotation keywords do not change validation semantics.
            "title" | "description" | "default" | "examples" | "deprecated" | "readOnly"
            | "writeOnly" | "$comment" => {}
            "type" => {
                types = Some(parse_types(value, path)?);
            }
            "properties" => {
                let properties = value
                    .as_object()
                    .ok_or_else(|| ValidationError::new(path, "properties must be an object"))?;
                if properties.len() > MAX_OBJECT_PROPERTIES {
                    return Err(ValidationError::new(
                        path,
                        "properties exceeds the schema property limit",
                    ));
                }
                for (name, child) in properties {
                    if name.is_empty() || name.chars().count() > MAX_PROPERTY_NAME_CHARS {
                        return Err(ValidationError::new(
                            path,
                            "property name is empty or too long",
                        ));
                    }
                    let child_path = property_path(&format!("{path}.properties"), name);
                    check_schema_at(child, &child_path, depth + 1, nodes)?;
                }
            }
            "required" => {
                let required = value.as_array().ok_or_else(|| {
                    ValidationError::new(path, "required must be an array of property names")
                })?;
                if required.len() > MAX_OBJECT_PROPERTIES {
                    return Err(ValidationError::new(
                        path,
                        "required exceeds the property limit",
                    ));
                }
                let mut seen = std::collections::BTreeSet::new();
                for name in required {
                    let name = name.as_str().ok_or_else(|| {
                        ValidationError::new(path, "required entries must be strings")
                    })?;
                    if name.chars().count() > MAX_PROPERTY_NAME_CHARS || !seen.insert(name) {
                        return Err(ValidationError::new(
                            path,
                            "required contains a duplicate or overlong property name",
                        ));
                    }
                }
            }
            "additionalProperties" | "items" => {
                if keyword == "items" && value.is_array() {
                    return Err(ValidationError::new(
                        path,
                        "items tuple form is not supported; use one items schema",
                    ));
                }
                if !matches!(value, Value::Bool(_) | Value::Object(_)) {
                    return Err(ValidationError::new(
                        path,
                        format!("{keyword} must be a boolean or schema"),
                    ));
                }
                if value.is_object() || keyword == "items" {
                    check_schema_at(value, &format!("{path}.{keyword}"), depth + 1, nodes)?;
                }
            }
            "enum" => {
                let values = value
                    .as_array()
                    .ok_or_else(|| ValidationError::new(path, "enum must be an array"))?;
                if values.len() > MAX_ENUM_VALUES {
                    return Err(ValidationError::new(path, "enum value limit exceeded"));
                }
                if !crate::json::fits_bytes(value, MAX_ENUM_BYTES) {
                    return Err(ValidationError::new(path, "enum payload limit exceeded"));
                }
            }
            "const" => {
                if !crate::json::fits_bytes(value, MAX_CONST_BYTES) {
                    return Err(ValidationError::new(path, "const payload limit exceeded"));
                }
            }
            "minLength" | "maxLength" | "minItems" | "maxItems" | "minProperties"
            | "maxProperties" => {
                parse_bound(value, keyword, path)?;
            }
            "minimum" | "maximum" => {
                if value.as_f64().is_none() {
                    return Err(ValidationError::new(
                        path,
                        format!("{keyword} must be a number"),
                    ));
                }
            }
            _ => {
                return Err(ValidationError::new(path, "unsupported schema keyword"));
            }
        }
    }

    // Each typed keyword must have an explicit compatible type.
    for (keys, allowed, message) in [
        (
            &["properties"][..],
            &["object"][..],
            "properties requires type object",
        ),
        (
            &["required"][..],
            &["object"][..],
            "required requires type object",
        ),
        (
            &["additionalProperties"][..],
            &["object"][..],
            "additionalProperties requires type object",
        ),
        (&["items"][..], &["array"][..], "items requires type array"),
        (
            &["minLength", "maxLength"][..],
            &["string"][..],
            "length bounds require type string",
        ),
        (
            &["minItems", "maxItems"][..],
            &["array"][..],
            "item bounds require type array",
        ),
        (
            &["minProperties", "maxProperties"][..],
            &["object"][..],
            "property bounds require type object",
        ),
        (
            &["minimum", "maximum"][..],
            &["number", "integer"][..],
            "numeric bounds require type number or integer",
        ),
    ] {
        if keys.iter().any(|key| object.contains_key(*key)) {
            let types = types.as_ref().ok_or_else(|| {
                ValidationError::new(path, "typed schema keywords require an explicit type")
            })?;
            if !types
                .iter()
                .any(|kind| allowed.contains(&kind.as_str().unwrap()))
            {
                return Err(ValidationError::new(path, message));
            }
        }
    }
    for (min, max) in [
        ("minLength", "maxLength"),
        ("minItems", "maxItems"),
        ("minProperties", "maxProperties"),
        ("minimum", "maximum"),
    ] {
        if object
            .get(min)
            .and_then(Value::as_f64)
            .zip(object.get(max).and_then(Value::as_f64))
            .is_some_and(|(min, max)| min > max)
        {
            return Err(ValidationError::new(path, format!("{min} exceeds {max}")));
        }
    }

    Ok(())
}

fn validate_bounds(
    schema: &Value,
    min: &str,
    max: &str,
    value: f64,
    path: &str,
) -> Result<(), ValidationError> {
    for (key, lower) in [(min, true), (max, false)] {
        if schema
            .get(key)
            .and_then(Value::as_f64)
            .is_some_and(|bound| if lower { value < bound } else { value > bound })
        {
            return Err(ValidationError::new(path, format!("{key} constraint")));
        }
    }
    Ok(())
}

fn parse_types<'a>(value: &'a Value, path: &str) -> Result<&'a [Value], ValidationError> {
    let names = match value {
        Value::String(_) => std::slice::from_ref(value),
        Value::Array(items) if !items.is_empty() && items.len() <= 7 => items,
        _ => {
            return Err(ValidationError::new(
                path,
                "type must be a string or a non-empty array of strings",
            ))
        }
    };
    if names.iter().any(|name| !name.is_string()) {
        return Err(ValidationError::new(path, "type entries must be strings"));
    }
    for (index, name) in names.iter().enumerate() {
        if !matches!(
            name.as_str().unwrap(),
            "object" | "array" | "string" | "number" | "integer" | "boolean" | "null"
        ) {
            return Err(ValidationError::new(path, "unsupported type name"));
        }
        if names[..index].contains(name) {
            return Err(ValidationError::new(path, "duplicate type name"));
        }
    }
    Ok(names)
}

fn parse_bound(value: &Value, keyword: &str, path: &str) -> Result<u64, ValidationError> {
    let bound = value.as_u64().ok_or_else(|| {
        ValidationError::new(path, format!("{keyword} must be a non-negative integer"))
    })?;
    if bound > MAX_NUMERIC_BOUND {
        return Err(ValidationError::new(
            path,
            format!("{keyword} exceeds the supported bound"),
        ));
    }
    Ok(bound)
}

fn validate_at(
    schema: &Value,
    instance: &Value,
    path: &str,
    depth: usize,
    nodes: &mut usize,
) -> Result<(), ValidationError> {
    *nodes += 1;
    if *nodes > MAX_INSTANCE_NODES {
        return Err(ValidationError::new(path, "instance node limit exceeded"));
    }
    if depth > MAX_INSTANCE_DEPTH {
        return Err(ValidationError::new(path, "instance depth limit exceeded"));
    }

    // Global input bounds apply even when a schema does not constrain the value.
    match instance {
        Value::String(text) if text.chars().count() > MAX_STRING_CHARS => {
            return Err(ValidationError::new(path, "input string limit exceeded"))
        }
        Value::Array(items) if items.len() > MAX_ARRAY_ITEMS => {
            return Err(ValidationError::new(path, "input array limit exceeded"))
        }
        Value::Object(object) if object.len() > MAX_OBJECT_PROPERTIES => {
            return Err(ValidationError::new(path, "input object limit exceeded"))
        }
        _ => {}
    }

    let object = match schema {
        Value::Bool(allowed) => {
            return if *allowed {
                Ok(())
            } else {
                Err(ValidationError::new(path, "schema is false"))
            }
        }
        Value::Object(object) => object,
        _ => {
            return Err(ValidationError::new(
                path,
                "schema must be an object or boolean",
            ))
        }
    };

    if let Some(type_value) = object.get("type") {
        // The immutable schema was checked at the public boundary.
        let types = type_value.as_str().into_iter().chain(
            type_value
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
        );
        if !types.clone().any(|kind| matches_type(kind, instance)) {
            return Err(ValidationError::new(
                path,
                format!(
                    "type must be one of: {}",
                    types.collect::<Vec<_>>().join(", ")
                ),
            ));
        }
    }

    if let Some(allowed) = object.get("enum").and_then(Value::as_array) {
        if !allowed.iter().any(|candidate| candidate == instance) {
            return Err(ValidationError::new(path, "value is not in enum"));
        }
    }
    if let Some(constant) = object.get("const") {
        if constant != instance {
            return Err(ValidationError::new(path, "value does not equal const"));
        }
    }

    match instance {
        Value::Object(values) => {
            validate_bounds(
                schema,
                "minProperties",
                "maxProperties",
                values.len() as f64,
                path,
            )?;
            if let Some(required) = object.get("required").and_then(Value::as_array) {
                for name in required {
                    let name = name.as_str().unwrap_or("");
                    if !values.contains_key(name) {
                        return Err(ValidationError::new(
                            property_path(path, name),
                            "required property is missing",
                        ));
                    }
                }
            }
            let properties = object.get("properties").and_then(Value::as_object);
            if let Some(properties) = properties {
                for (name, child_schema) in properties {
                    if let Some(child) = values.get(name) {
                        validate_at(
                            child_schema,
                            child,
                            &property_path(path, name),
                            depth + 1,
                            nodes,
                        )?;
                    }
                }
            }
            let mut extra = values.iter().filter(|(name, _)| {
                !properties.is_some_and(|properties| properties.contains_key(*name))
            });
            match object.get("additionalProperties") {
                Some(Value::Bool(false)) if extra.next().is_some() => {
                    return Err(ValidationError::new(
                        path,
                        "additionalProperties: unexpected property is not allowed",
                    ));
                }
                Some(additional @ Value::Object(_)) => {
                    for (_, child) in extra {
                        validate_at(additional, child, &format!("{path}.*"), depth + 1, nodes)?;
                    }
                }
                _ => {}
            }
        }
        Value::Array(items) => {
            validate_bounds(schema, "minItems", "maxItems", items.len() as f64, path)?;
            if let Some(child_schema) = object.get("items") {
                for (index, child) in items.iter().enumerate() {
                    validate_at(
                        child_schema,
                        child,
                        &format!("{path}[{index}]"),
                        depth + 1,
                        nodes,
                    )?;
                }
            }
        }
        Value::String(text) => validate_bounds(
            schema,
            "minLength",
            "maxLength",
            text.chars().count() as f64,
            path,
        )?,
        Value::Number(number) => {
            validate_bounds(schema, "minimum", "maximum", number.as_f64().unwrap(), path)?
        }
        _ => {}
    }
    Ok(())
}

fn matches_type(kind: &str, instance: &Value) -> bool {
    match kind {
        "object" => instance.is_object(),
        "array" => instance.is_array(),
        "string" => instance.is_string(),
        "number" => instance.is_number(),
        "integer" => instance
            .as_number()
            .is_some_and(|number| number.is_i64() || number.is_u64()),
        "boolean" => instance.is_boolean(),
        "null" => instance.is_null(),
        _ => false,
    }
}

fn property_path(parent: &str, name: &str) -> String {
    if !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        && !name.chars().next().is_some_and(|ch| ch.is_ascii_digit())
    {
        format!("{parent}.{name}")
    } else {
        let encoded = serde_json::to_string(name).unwrap_or_else(|_| "\"?\"".into());
        format!("{parent}[{encoded}]")
    }
}

fn truncate_path(path: String) -> String {
    truncate_text(path, MAX_PATH_CHARS)
}

fn truncate_text(text: String, limit: usize) -> String {
    if text.chars().count() <= limit {
        text
    } else {
        let mut out = text.chars().take(limit).collect::<String>();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn representative_schema() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "actions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 4,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "action": {"type": "string", "enum": ["create", "update"]},
                            "title": {"type": "string", "maxLength": 10},
                            "confidence": {"type": "number", "minimum": 0, "maximum": 1},
                            "fields": {
                                "type": "array",
                                "maxItems": 2,
                                "items": {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "properties": {
                                        "field": {"type": "string", "enum": ["title", "due"]},
                                        "value": {"type": "string", "maxLength": 6}
                                    },
                                    "required": ["field", "value"]
                                }
                            }
                        },
                        "required": ["action", "fields"]
                    }
                }
            },
            "required": ["actions"]
        })
    }

    #[test]
    fn accepts_representative_nested_schema_and_instance() {
        let schema = representative_schema();
        check_schema(&schema).unwrap();
        validate(
            &schema,
            &json!({"actions":[
                {"action":"create","title":"修复","confidence":0.9,"fields":[
                    {"field":"title","value":"甲"},
                    {"field":"due","value":"明天"}
                ]}
            ]}),
        )
        .unwrap();
    }

    #[test]
    fn rejects_missing_required_extra_property_and_enum_or_bounds() {
        let schema = representative_schema();
        let cases = [
            (json!({}), "required"),
            (
                json!({"actions":[{"action":"create","fields":[]}],"extra":1}),
                "additionalProperties",
            ),
            (json!({"actions":[{"action":"ignore","fields":[]}]}), "enum"),
            (
                json!({"actions":[{"action":"create","title":"超出长度限制的内容测试","fields":[]}]}),
                "maxLength",
            ),
            (
                json!({"actions":[{"action":"create","confidence":2,"fields":[]}]}),
                "maximum",
            ),
            (
                json!({"actions":[{"action":"create","fields":[{"field":"title"}]}]}),
                "required",
            ),
        ];
        for (instance, expected) in cases {
            let error = validate(&schema, &instance).unwrap_err();
            assert!(
                error.constraint().contains(expected),
                "expected {expected}, got {error}"
            );
        }
    }

    #[test]
    fn unsupported_validation_keywords_fail_closed() {
        for schema in [
            json!({"type":"string","pattern":"^a"}),
            json!({"type":"string","format":"date-time"}),
            json!({"oneOf":[{"type":"string"},{"type":"number"}]}),
            json!({"$ref":"#/definitions/x"}),
            json!({"type":"array","items":[{"type":"string"}]}),
            json!({"allOf":[{"type":"string"}]}),
            json!({"if":{"type":"string"},"then":{"type":"number"}}),
        ] {
            let error = check_schema(&schema).unwrap_err();
            assert!(
                error.constraint().contains("unsupported")
                    || error.constraint().contains("tuple form")
            );
        }
    }

    #[test]
    fn unsupported_keyword_is_sanitized_and_does_not_echo_input() {
        let schema = json!({"type":"string","x-secret-token":"not-a-keyword"});
        let error = check_schema(&schema).unwrap_err();
        assert!(error.constraint().contains("unsupported schema keyword"));
        assert!(!error.to_string().contains("x-secret-token"));
    }

    #[test]
    fn errors_do_not_echo_input_values_or_unknown_property_names() {
        let schema = json!({
            "type":"object",
            "additionalProperties":false,
            "properties":{"public":{"type":"string","maxLength":3}}
        });
        let secret = "TOPSECRET-VALUE-9f4a";
        let error = validate(&schema, &json!({"public": secret})).unwrap_err();
        assert!(!error.to_string().contains(secret));
        assert!(error.constraint().contains("maxLength"));

        let error = validate(&schema, &json!({"TOPSECRET-KEY": 1})).unwrap_err();
        assert!(!error.to_string().contains("TOPSECRET-KEY"));
        assert!(error.constraint().contains("additionalProperties"));
    }

    #[test]
    fn unicode_length_and_integer_types_are_exact() {
        let schema = json!({"type":"string","maxLength":2,"minLength":2});
        validate(&schema, &json!("中🙂")).unwrap();
        assert!(validate(&schema, &json!("中🙂x")).is_err());

        let integer = json!({"type":"integer","minimum":1,"maximum":7});
        validate(&integer, &json!(7)).unwrap();
        assert!(validate(&integer, &json!(7.0)).is_err());
        assert!(validate(&integer, &json!(8)).is_err());
    }

    #[test]
    fn additional_properties_schema_and_nullable_type_arrays_work() {
        let schema = json!({
            "type":"object",
            "additionalProperties":{"type":"string","maxLength":4}
        });
        validate(&schema, &json!({"a":"ok"})).unwrap();
        assert!(validate(&schema, &json!({"a":1})).is_err());

        let nullable = json!({"type":["string","null"],"maxLength":2});
        validate(&nullable, &json!(null)).unwrap();
        validate(&nullable, &json!("中")).unwrap();
        assert!(validate(&nullable, &json!("中xy")).is_err());
    }

    #[test]
    fn annotation_keywords_are_accepted() {
        let schema = json!({
            "title":"T",
            "description":"D",
            "default":{},
            "examples":[1],
            "deprecated":false,
            "readOnly":true,
            "writeOnly":false,
            "$comment":"c",
            "type":"object",
            "properties":{},
            "additionalProperties":false
        });
        check_schema(&schema).unwrap();
        validate(&schema, &json!({})).unwrap();
    }

    #[test]
    fn depth_and_node_bounds_stop_recursion() {
        let mut deep_schema = json!({"type":"integer"});
        let mut deep_instance = json!(1);
        for _ in 0..70 {
            deep_schema = json!({"type":"array","items":deep_schema});
            deep_instance = json!([deep_instance]);
        }
        // The public entry point rejects the over-deep schema first.
        assert!(check_schema(&deep_schema)
            .unwrap_err()
            .constraint()
            .contains("depth limit"));
        // Exercise the instance-depth guard directly with an over-deep schema
        // that bypasses the schema preflight.
        let mut nodes = 0;
        let error = validate_at(&deep_schema, &deep_instance, "$", 0, &mut nodes).unwrap_err();
        assert!(error.constraint().contains("depth limit"));

        let wide_schema = json!({
            "type":"array",
            "items":{"type":"array","items":{"type":"integer"}}
        });
        let rows = (0..100).map(|_| json!(vec![1; 2_100])).collect::<Vec<_>>();
        let error = validate(&wide_schema, &json!(rows)).unwrap_err();
        assert!(
            error.constraint().contains("node limit") || error.constraint().contains("array limit")
        );
    }

    #[test]
    fn schema_structural_errors_fail_closed() {
        for schema in [
            json!({"type":"bogus"}),
            json!({"type":"string","maxLength":-1}),
            json!({"type":"string","minLength":4,"maxLength":2}),
            json!({"type":"array","items":{"type":"string"},"minItems":4,"maxItems":2}),
            json!({"type":"object","required":"title"}),
            json!({"type":"object","properties":[]}),
            json!({"properties":{"a":{"type":"string"}}}),
        ] {
            assert!(check_schema(&schema).is_err(), "schema: {schema}");
        }
    }

    #[test]
    fn boolean_schemas_are_supported() {
        validate(&json!(true), &json!({"any":1})).unwrap();
        assert!(validate(&json!(false), &json!(1)).is_err());
    }

    #[test]
    fn object_property_bounds_and_const_are_enforced() {
        let schema = json!({
            "type":"object",
            "minProperties":1,
            "maxProperties":1,
            "additionalProperties":{"const":"ok"}
        });
        validate(&schema, &json!({"a":"ok"})).unwrap();
        assert!(validate(&schema, &json!({})).is_err());
        assert!(validate(&schema, &json!({"a":"ok","b":"ok"})).is_err());
        assert!(validate(&schema, &json!({"a":"bad"})).is_err());
    }
}
