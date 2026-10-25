//! Conversion of tool-call argument values to the types their tool's schema
//! declares, for a call that failed its arguments.

use serde_json::{Map, Value};

/// Rewrites the values of `args` to the types the properties of `schema`
/// declare, and returns the new JSON text when something changed.
///
/// A value of another type is what most `invalid arguments` results come to:
/// a number sent as `"5"`, one edit sent instead of a list of them, `true`
/// sent as `"true"`. The caller runs this only after that failure, so a
/// well-formed call pays nothing.
pub fn by_schema(args: &str, schema: &Value) -> Option<String> {
    let mut value: Value = serde_json::from_str(args).ok()?;
    let Value::Object(ref mut object) = value else {
        return None;
    };
    let changed = convert_object(object, schema);
    changed.then(|| value.to_string())
}

/// Converts the values of `object` by the `properties` of `schema`, and drops
/// a null the call may leave out. Returns whether anything changed.
fn convert_object(object: &mut Map<String, Value>, schema: &Value) -> bool {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return false;
    };
    let required = schema.get("required").and_then(Value::as_array);
    let mut changed = false;
    object.retain(|key, value| {
        let Some(property) = properties.get(key) else {
            return true;
        };
        if !value.is_null() {
            changed |= convert(value, property);
            return true;
        }
        // A null for a required key is the call the model sent: keep it.
        let required = required.is_some_and(|required| {
            required
                .iter()
                .any(|name| name.as_str() == Some(key.as_str()))
        });
        changed |= !required;
        required
    });
    changed
}

/// Replaces `value` with its conversion to the first type `schema` names that
/// it can take, then by that type's `items` or `properties`. Returns whether
/// anything changed.
fn convert(value: &mut Value, schema: &Value) -> bool {
    let Some(converted) = typed(value, schema) else {
        return false;
    };
    if *value == converted {
        return false;
    }
    *value = converted;
    true
}

/// The conversion of `value` to the first type `schema` names that applies
/// to it; `None` when no type does. A type is a name or a list of names.
fn typed(value: &Value, schema: &Value) -> Option<Value> {
    let types = schema.get("type")?;
    let names = match types {
        Value::Array(names) => names.as_slice(),
        Value::String(_) => std::slice::from_ref(types),
        _ => return None,
    };
    names
        .iter()
        .filter_map(Value::as_str)
        .find_map(|name| by_type(value, name, schema))
}

/// The conversion of `value` to `name`, when `name` says what it already is
/// or what it can be read as.
fn by_type(value: &Value, name: &str, schema: &Value) -> Option<Value> {
    match name {
        "integer" => integer(value),
        "number" => number(value),
        "boolean" => boolean(value),
        "string" => string(value),
        "array" => array(value, schema),
        "object" => object(value, schema),
        _ => None,
    }
}

/// A string that parses as an `i64`, or a float with no fraction, as an
/// integer.
fn integer(value: &Value) -> Option<Value> {
    match value {
        Value::String(text) => text.parse::<i64>().ok().map(Value::from),
        Value::Number(number) if number.is_f64() => {
            let float = number.as_f64()?;
            let whole = (i64::MIN as f64..i64::MAX as f64).contains(&float) && float.fract() == 0.0;
            whole.then_some(Value::from(float as i64))
        }
        _ => None,
    }
}

/// A string that parses as a number, as that number.
fn number(value: &Value) -> Option<Value> {
    let Value::String(text) = value else {
        return None;
    };
    text.parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map(Value::Number)
}

/// The strings `true` and `false`, in any ASCII case, as their bool.
fn boolean(value: &Value) -> Option<Value> {
    let Value::String(text) = value else {
        return None;
    };
    if text.eq_ignore_ascii_case("true") {
        return Some(Value::Bool(true));
    }
    text.eq_ignore_ascii_case("false")
        .then_some(Value::Bool(false))
}

/// A number or a bool, as the JSON text it prints as.
fn string(value: &Value) -> Option<Value> {
    match value {
        Value::Number(number) => Some(Value::String(number.to_string())),
        Value::Bool(flag) => Some(Value::String(flag.to_string())),
        _ => None,
    }
}

/// A string that parses as an array, else any value but null, as a
/// one-element array; each item is then converted by the schema's `items`.
fn array(value: &Value, schema: &Value) -> Option<Value> {
    let values = match value {
        Value::Array(values) => values.clone(),
        Value::String(text) => match serde_json::from_str::<Value>(text) {
            Ok(Value::Array(values)) => values,
            _ => vec![value.clone()],
        },
        Value::Null => return None,
        value => vec![value.clone()],
    };
    let Some(items) = schema.get("items").filter(|items| items.is_object()) else {
        return Some(Value::Array(values));
    };
    Some(Value::Array(
        values
            .into_iter()
            .map(|mut item| {
                convert(&mut item, items);
                item
            })
            .collect(),
    ))
}

/// A string that parses as an object, as that object converted by the
/// schema's `properties`. A value that is no object stays as it is.
fn object(value: &Value, schema: &Value) -> Option<Value> {
    let mut object = match value {
        Value::Object(object) => object.clone(),
        Value::String(text) => match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(object)) => object,
            _ => return None,
        },
        _ => return None,
    };
    convert_object(&mut object, schema);
    Some(Value::Object(object))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn schema(properties: Value, required: Value) -> Value {
        json!({ "type": "object", "properties": properties, "required": required })
    }

    fn coerced(args: Value, schema: &Value) -> Option<Value> {
        by_schema(&args.to_string(), schema)
            .map(|text| serde_json::from_str(&text).expect("the converted arguments parse"))
    }

    /// The schema of the `edit` tool, with an array of objects of its own.
    fn edits_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": { "type": "string" },
                            "new_string": { "type": "string" },
                            "line": { "type": "integer" },
                            "replace_all": { "type": "boolean" }
                        },
                        "required": ["old_string", "new_string"]
                    }
                }
            },
            "required": ["path"]
        })
    }

    #[test]
    fn a_string_integer_becomes_a_number() {
        let schema = schema(json!({ "limit": { "type": "integer" } }), json!([]));
        assert_eq!(
            coerced(json!({ "limit": "5" }), &schema),
            Some(json!({ "limit": 5 }))
        );
        assert_eq!(coerced(json!({ "limit": "5.5" }), &schema), None);
    }

    #[test]
    fn a_whole_float_becomes_an_integer() {
        let schema = schema(json!({ "line": { "type": "integer" } }), json!([]));
        assert_eq!(
            coerced(json!({ "line": 3.0 }), &schema),
            Some(json!({ "line": 3 }))
        );
        assert_eq!(coerced(json!({ "line": 3.5 }), &schema), None);
    }

    #[test]
    fn a_string_number_becomes_a_number() {
        let schema = schema(json!({ "score": { "type": "number" } }), json!([]));
        assert_eq!(
            coerced(json!({ "score": "2.5" }), &schema),
            Some(json!({ "score": 2.5 }))
        );
        assert_eq!(coerced(json!({ "score": "high" }), &schema), None);
    }

    #[test]
    fn a_string_boolean_becomes_a_bool() {
        let schema = schema(json!({ "replace_all": { "type": "boolean" } }), json!([]));
        assert_eq!(
            coerced(json!({ "replace_all": "TRUE" }), &schema),
            Some(json!({ "replace_all": true }))
        );
        assert_eq!(
            coerced(json!({ "replace_all": "false" }), &schema),
            Some(json!({ "replace_all": false }))
        );
        assert_eq!(coerced(json!({ "replace_all": "yes" }), &schema), None);
    }

    #[test]
    fn a_string_array_is_parsed_or_wrapped() {
        let schema = schema(
            json!({ "paths": { "type": "array", "items": { "type": "string" } } }),
            json!([]),
        );
        assert_eq!(
            coerced(json!({ "paths": "[\"a.rs\",\"b.rs\"]" }), &schema),
            Some(json!({ "paths": ["a.rs", "b.rs"] }))
        );
        assert_eq!(
            coerced(json!({ "paths": "a.rs" }), &schema),
            Some(json!({ "paths": ["a.rs"] }))
        );
        assert_eq!(
            coerced(json!({ "paths": 5 }), &schema),
            Some(json!({ "paths": ["5"] }))
        );
    }

    #[test]
    fn a_string_object_is_parsed() {
        let schema = schema(
            json!({ "opts": { "type": "object", "properties": { "n": { "type": "integer" } } } }),
            json!([]),
        );
        assert_eq!(
            coerced(json!({ "opts": "{\"n\":\"2\"}" }), &schema),
            Some(json!({ "opts": { "n": 2 } }))
        );
        assert_eq!(coerced(json!({ "opts": "[]" }), &schema), None);
    }

    #[test]
    fn a_number_or_bool_becomes_a_string() {
        let schema = schema(json!({ "path": { "type": "string" } }), json!([]));
        assert_eq!(
            coerced(json!({ "path": 5 }), &schema),
            Some(json!({ "path": "5" }))
        );
        assert_eq!(
            coerced(json!({ "path": true }), &schema),
            Some(json!({ "path": "true" }))
        );
        assert_eq!(coerced(json!({ "path": "a.rs" }), &schema), None);
    }

    #[test]
    fn a_type_list_takes_the_first_that_applies() {
        let schema = schema(
            json!({ "line": { "type": ["integer", "string"] } }),
            json!([]),
        );
        assert_eq!(
            coerced(json!({ "line": "5" }), &schema),
            Some(json!({ "line": 5 }))
        );
        assert_eq!(
            coerced(json!({ "line": true }), &schema),
            Some(json!({ "line": "true" }))
        );
    }

    #[test]
    fn an_edits_array_converts_the_values_of_its_objects() {
        let args = json!({
            "path": "a.rs",
            "edits": [{ "old_string": "a", "new_string": "b", "line": "3", "replace_all": "true" }]
        });
        assert_eq!(
            coerced(args, &edits_schema()),
            Some(json!({
                "path": "a.rs",
                "edits": [{ "old_string": "a", "new_string": "b", "line": 3, "replace_all": true }]
            }))
        );
    }

    #[test]
    fn typed_values_and_unknown_keys_stay_as_they_are() {
        let schema = schema(json!({ "limit": { "type": "integer" } }), json!([]));
        assert_eq!(coerced(json!({ "limit": 5 }), &schema), None);
        assert_eq!(coerced(json!({ "other": "5" }), &schema), None);
        assert_eq!(by_schema("not json", &schema), None);
        assert_eq!(by_schema("[1]", &schema), None);
    }

    #[test]
    fn a_required_null_is_kept() {
        let schema = schema(
            json!({ "path": { "type": "string" }, "limit": { "type": "integer" } }),
            json!(["path"]),
        );
        assert_eq!(
            coerced(json!({ "path": "a.rs", "limit": null }), &schema),
            Some(json!({ "path": "a.rs" }))
        );
        assert_eq!(
            coerced(json!({ "path": null, "limit": null }), &schema),
            Some(json!({ "path": null }))
        );
    }
}
