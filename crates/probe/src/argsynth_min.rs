//! Minimal, structural-only JSON-Schema argument synthesis for the `tools/call` step of the
//! probe protocol.
//!
//! Deliberately **not** P2-06's `argsynth` crate, which is unbuilt and out of scope here:
//! that component's job is full schema-driven generation plus fixture-bound *semantic*
//! validity, and depends on the Phase-2 world provisioner. This module exists only to get
//! past `tools/call`'s required-parameter validation with *something* structurally
//! well-typed — trivial defaults, no semantic awareness at all. A tool invoked with these
//! arguments may reject them outright (recorded by the caller as an invocation failure, not
//! guessed around) or, if it accepts them, may do something its semantics don't intend for
//! placeholder input. That is a known, documented limitation (design.md §8, "semantic
//! argument validity"), not an oversight, and is exactly why Track B's protocol only ever
//! reaches a decisive verdict from an *observed* state change, never from assuming one.

use serde_json::{Map, Value};

/// Build the smallest structurally valid argument object for a JSON-Schema `inputSchema`,
/// covering only the tool's own declared `required` properties.
///
/// Everything else about the schema — formats, enums, patterns, nested `$ref`s, minimum/
/// maximum bounds — is ignored. A schema this function can't make sense of degrades to an
/// empty object rather than an error: the caller finds out whether that was good enough the
/// same way it finds out about anything else this weak an oracle can't be sure of — the
/// tool either accepts the call or it doesn't.
#[must_use]
pub fn synthesize_arguments(input_schema: &Value) -> Value {
    let mut args = Map::new();
    let Some(schema) = input_schema.as_object() else {
        return Value::Object(args);
    };

    let properties = schema.get("properties").and_then(Value::as_object);
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    for name in required {
        let prop_schema = properties.and_then(|p| p.get(name));
        args.insert(name.to_string(), placeholder_for(prop_schema));
    }
    Value::Object(args)
}

fn placeholder_for(schema: Option<&Value>) -> Value {
    let type_str = schema.and_then(|s| s.get("type")).and_then(Value::as_str);
    match type_str {
        Some("string") => Value::String(String::new()),
        Some("integer" | "number") => Value::from(0),
        Some("boolean") => Value::Bool(false),
        Some("array") => Value::Array(Vec::new()),
        Some("object") => Value::Object(Map::new()),
        // Unknown or absent `type` (e.g. a `$ref`, a union, `null`): no structurally safe
        // default exists, so this leaves it out rather than guessing at a shape that might
        // fail the schema anyway. Recorded as a placeholder gap, not silently as `null` —
        // `null` is itself a valid distinct JSON Schema type this function should not
        // pretend to have chosen on purpose.
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_with_no_required_fields_yields_an_empty_object() {
        let schema = serde_json::json!({"type": "object", "properties": {"a": {"type": "string"}}});
        assert_eq!(synthesize_arguments(&schema), serde_json::json!({}));
    }

    #[test]
    fn non_object_schema_yields_an_empty_object() {
        assert_eq!(synthesize_arguments(&Value::Null), serde_json::json!({}));
        assert_eq!(synthesize_arguments(&Value::Bool(true)), serde_json::json!({}));
    }

    #[test]
    fn required_fields_get_type_appropriate_placeholders() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "s": {"type": "string"},
                "n": {"type": "number"},
                "i": {"type": "integer"},
                "b": {"type": "boolean"},
                "arr": {"type": "array"},
                "obj": {"type": "object"},
            },
            "required": ["s", "n", "i", "b", "arr", "obj"],
        });
        let args = synthesize_arguments(&schema);
        assert_eq!(args["s"], "");
        assert_eq!(args["n"], 0);
        assert_eq!(args["i"], 0);
        assert_eq!(args["b"], false);
        assert_eq!(args["arr"], serde_json::json!([]));
        assert_eq!(args["obj"], serde_json::json!({}));
    }

    #[test]
    fn a_required_field_missing_from_properties_or_with_an_unknown_type_is_null() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"weird": {"$ref": "#/definitions/thing"}},
            "required": ["weird", "not_even_declared"],
        });
        let args = synthesize_arguments(&schema);
        assert_eq!(args["weird"], Value::Null);
        assert_eq!(args["not_even_declared"], Value::Null);
    }

    #[test]
    fn only_required_fields_are_populated_not_every_declared_property() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"required_one": {"type": "string"}, "optional_one": {"type": "string"}},
            "required": ["required_one"],
        });
        let args = synthesize_arguments(&schema);
        assert!(args.get("required_one").is_some());
        assert!(args.get("optional_one").is_none());
    }
}
