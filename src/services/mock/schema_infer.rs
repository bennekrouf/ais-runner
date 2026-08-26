//! Generate an example JSON body from a JSON Schema fragment.
//!
//! Used by the scanner to build a "ready-to-return" stub response per endpoint.
//! Defaults are deliberately empty-but-valid:
//!   - `string`              → `""`
//!   - `string/date-time`    → `"1970-01-01T00:00:00Z"`
//!   - `string/uuid`         → `"00000000-…-0"`
//!   - `number` / `integer`  → `0`
//!   - `boolean`             → `false`
//!   - `array`               → `[<one example item>]`
//!   - `object`              → `{ <all declared props> }`
//!   - missing/null type     → `null`

use serde_json::{json, Map, Value};

/// Build an example value matching the given schema.
pub fn example_from(schema: &Value) -> Value {
    let ty = schema.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match ty {
        "object" => {
            let mut obj = Map::new();
            if let Some(props) = schema.get("properties").and_then(|v| v.as_object()) {
                for (k, sub) in props {
                    obj.insert(k.clone(), example_from(sub));
                }
            }
            Value::Object(obj)
        }
        "array" => {
            let item_schema = schema.get("items").cloned().unwrap_or(Value::Null);
            json!([example_from(&item_schema)])
        }
        "string" => match schema.get("format").and_then(|v| v.as_str()) {
            Some("date-time") => json!("1970-01-01T00:00:00Z"),
            Some("uuid") => json!("00000000-0000-0000-0000-000000000000"),
            Some("email") => json!("user@example.com"),
            _ => json!(""),
        },
        "number" | "integer" => json!(0),
        "boolean" => json!(false),
        "null" => Value::Null,
        _ => {
            // Permissive union types: ["string","null"] etc. — pick the first non-null.
            if let Some(arr) = schema.get("type").and_then(|v| v.as_array()) {
                if let Some(first) = arr.iter().find(|v| v.as_str() != Some("null")) {
                    let mut narrowed = schema.clone();
                    narrowed["type"] = first.clone();
                    return example_from(&narrowed);
                }
            }
            Value::Null
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_with_string_and_int() {
        let s = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "age":  { "type": "integer" }
            }
        });
        assert_eq!(example_from(&s), json!({ "name": "", "age": 0 }));
    }

    #[test]
    fn nested_array_of_objects() {
        let s = json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": { "id": { "type": "string", "format": "uuid" } }
            }
        });
        assert_eq!(
            example_from(&s),
            json!([{ "id": "00000000-0000-0000-0000-000000000000" }])
        );
    }

    #[test]
    fn union_type_picks_first_non_null() {
        let s = json!({ "type": ["string", "null"] });
        assert_eq!(example_from(&s), json!(""));
    }
}
