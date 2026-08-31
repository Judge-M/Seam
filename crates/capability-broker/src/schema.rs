//! Deterministic validation of capability arguments.
//!
//! The broker treats SLM output as untrusted structured input, so arguments are checked
//! before a provider is ever invoked. This implements the subset of JSON Schema that
//! capability descriptors actually use — `type`, `required`, `properties`, `items`,
//! `enum` and `additionalProperties` — and deliberately nothing more. A capability
//! needing richer validation should validate inside its provider, or this should be
//! swapped for a full JSON Schema crate; it is a seam, not a schema engine.

use serde_json::Value;

/// Check `arguments` against `schema`. `Err` carries a human-readable path and reason.
pub fn validate(schema: &Value, arguments: &Value) -> Result<(), String> {
    validate_at("arguments", schema, arguments)
}

fn validate_at(path: &str, schema: &Value, value: &Value) -> Result<(), String> {
    let Some(schema) = schema.as_object() else {
        // A non-object schema constrains nothing.
        return Ok(());
    };

    if let Some(expected) = schema.get("type").and_then(Value::as_str)
        && !matches_type(expected, value)
    {
        return Err(format!(
            "{path}: expected {expected}, found {}",
            type_name(value)
        ));
    }

    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        return Err(format!("{path}: value is not one of the permitted options"));
    }

    if let Some(object) = value.as_object() {
        let properties = schema.get("properties").and_then(Value::as_object);

        for required in schema
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !object.contains_key(required) {
                return Err(format!("{path}: missing required property `{required}`"));
            }
        }

        if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
            for key in object.keys() {
                let known = properties.is_some_and(|properties| properties.contains_key(key));
                if !known {
                    return Err(format!("{path}: unexpected property `{key}`"));
                }
            }
        }

        if let Some(properties) = properties {
            for (key, child) in object {
                if let Some(child_schema) = properties.get(key) {
                    validate_at(&format!("{path}.{key}"), child_schema, child)?;
                }
            }
        }
    }

    if let (Some(items), Some(item_schema)) = (value.as_array(), schema.get("items")) {
        for (index, item) in items.iter().enumerate() {
            validate_at(&format!("{path}[{index}]"), item_schema, item)?;
        }
    }

    Ok(())
}

fn matches_type(expected: &str, value: &Value) -> bool {
    match expected {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        // JSON Schema treats any number as `number`; `integer` additionally requires no
        // fractional part, which `is_i64`/`is_u64` alone would get wrong for `2.0`.
        "number" => value.is_number(),
        "integer" => value.as_f64().is_some_and(|number| number.fract() == 0.0),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn search_schema() -> Value {
        json!({
            "type": "object",
            "properties": {"query": {"type": "string"}, "limit": {"type": "integer"}},
            "required": ["query"],
            "additionalProperties": false
        })
    }

    #[test]
    fn accepts_valid_arguments() {
        assert!(validate(&search_schema(), &json!({"query": "seam", "limit": 5})).is_ok());
    }

    #[test]
    fn rejects_missing_required_property() {
        let error = validate(&search_schema(), &json!({"limit": 5})).unwrap_err();
        assert!(
            error.contains("missing required property `query`"),
            "{error}"
        );
    }

    #[test]
    fn rejects_wrong_property_type() {
        let error = validate(&search_schema(), &json!({"query": 7})).unwrap_err();
        assert!(error.contains("arguments.query"), "{error}");
    }

    #[test]
    fn rejects_unknown_property_when_additional_properties_are_denied() {
        let error = validate(
            &search_schema(),
            &json!({"query": "seam", "shell": "rm -rf /"}),
        )
        .unwrap_err();
        assert!(error.contains("unexpected property `shell`"), "{error}");
    }

    #[test]
    fn rejects_non_object_arguments() {
        let error = validate(&search_schema(), &json!("seam")).unwrap_err();
        assert!(error.contains("expected object"), "{error}");
    }

    #[test]
    fn rejects_fractional_integer() {
        let error = validate(&search_schema(), &json!({"query": "s", "limit": 1.5})).unwrap_err();
        assert!(error.contains("expected integer"), "{error}");
    }

    #[test]
    fn validates_array_items() {
        let schema = json!({"type": "array", "items": {"type": "string"}});
        assert!(validate(&schema, &json!(["a", "b"])).is_ok());
        assert!(validate(&schema, &json!(["a", 2])).is_err());
    }

    #[test]
    fn enforces_enum_membership() {
        let schema = json!({"enum": ["read", "write"]});
        assert!(validate(&schema, &json!("read")).is_ok());
        assert!(validate(&schema, &json!("delete")).is_err());
    }
}
