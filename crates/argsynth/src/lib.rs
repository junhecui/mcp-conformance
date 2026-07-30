//! Schema-driven argument generation, fixture binding, and cache-busting variants.
//!
//! **Must not:** Reuse an argument across arms that must be identical without recording that it did.
//!
//! Contract: [architecture.md §3.1].
//!
//! # Scope: a JSON Schema subset, not a validator
//!
//! MCP's `inputSchema` (`discovery::pin` already treats it as opaque JSON for pinning
//! purposes) is JSON Schema. This crate is a *generator*, not a general-purpose validator:
//! it understands exactly the shapes needed to synthesise a structurally valid argument set
//! — `object`/`array`/`string`/`number`/`integer`/`boolean`/`null`, plus `enum`/`const`,
//! `required`, `minLength`, `minimum`/`maximum`, `minItems` — and returns a loud
//! [`SynthesisError::UnsupportedSchema`] for anything past that (a `oneOf`/`anyOf`/`$ref`
//! union, a positional-tuple `items` array, a schema with no `type`/`enum`/`const` at all),
//! rather than guessing at a shape it was never told to handle.
//!
//! # Deterministic by construction
//!
//! No randomness anywhere in this crate. [`synthesize`] over the same schema and bindings
//! always produces the same value — the same "byte-reproducible, no ambient state"
//! discipline `sandbox::base_layer` and `world` already apply to the base layer and its
//! fixtures, applied here to arguments instead. This is what lets `D1` and `D1'`
//! (architecture.md §4.1's independent-repeat arm) receive identical arguments with no
//! special case: calling `synthesize` twice over the same inputs already gives the same
//! result, with no separate "make these two match" step required.

use serde_json::{Map, Number, Value};
use std::collections::BTreeMap;

/// Why synthesising an argument value from a schema failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SynthesisError {
    /// The schema (or a nested subschema) used a shape this generator does not understand —
    /// named so a caller can tell "arguments could not be produced" apart from "produced
    /// empty/default arguments."
    UnsupportedSchema {
        /// Where in the schema (dotted property path; `[]` marks an array's `items`; empty
        /// string for the root) this was hit.
        at: String,
    },
}

impl std::fmt::Display for SynthesisError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedSchema { at } => {
                let location = if at.is_empty() { "schema root".to_string() } else { format!("`{at}`") };
                write!(f, "unsupported schema shape at {location}")
            }
        }
    }
}

impl std::error::Error for SynthesisError {}

/// Concrete values to substitute for named top-level properties, standing in for "an entity
/// that actually exists" in whatever fixture (P2-05's generic fixture today; a per-server
/// one once Phase 3 lands bespoke fixtures) this run is seeded against.
///
/// Deliberately keyed by property *name*, not inferred by any naming heuristic over the
/// schema itself — this crate does not guess that a property called `path` or `id` refers to
/// a fixture entity; the caller, who actually knows both the tool's schema and what the
/// fixture seeded, states the binding explicitly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FixtureBindings(BTreeMap<String, Value>);

impl FixtureBindings {
    /// An empty binding set — every property synthesises structurally; none are bound.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind `property` to `value` for every future [`synthesize`] call using this set.
    #[must_use]
    pub fn with(mut self, property: impl Into<String>, value: Value) -> Self {
        self.0.insert(property.into(), value);
        self
    }
}

/// The result of one [`synthesize`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynthesisResult {
    /// The generated arguments — the JSON object `tools/call` sends as `arguments`.
    pub arguments: Value,
    /// Dotted paths of every property substituted from [`FixtureBindings`] rather than
    /// generated structurally — recorded so a caller can tell "genuinely synthesised" and
    /// "bound to a real fixture entity" apart on inspection of the result alone, not only at
    /// the call site that happened to supply the bindings.
    pub fixture_bound_properties: Vec<String>,
}

/// Synthesise a structurally (and, where bound, semantically) valid argument value for
/// `schema`, an MCP tool's `inputSchema`.
///
/// `bindings` overrides properties by name at any nesting depth; every other property is
/// generated purely from the schema's own structure.
///
/// # Errors
///
/// Returns [`SynthesisError::UnsupportedSchema`] if `schema` (or a nested subschema) uses a
/// shape outside this crate's documented subset — see the module doc comment's "Scope"
/// section.
pub fn synthesize(
    schema: &Value,
    bindings: &FixtureBindings,
) -> Result<SynthesisResult, SynthesisError> {
    let mut fixture_bound_properties = Vec::new();
    let arguments = synthesize_at(schema, bindings, "", &mut fixture_bound_properties)?;
    Ok(SynthesisResult { arguments, fixture_bound_properties })
}

fn synthesize_at(
    schema: &Value,
    bindings: &FixtureBindings,
    path: &str,
    fixture_bound_properties: &mut Vec<String>,
) -> Result<Value, SynthesisError> {
    let object = schema
        .as_object()
        .ok_or_else(|| SynthesisError::UnsupportedSchema { at: path.to_string() })?;

    if let Some(constant) = object.get("const") {
        return Ok(constant.clone());
    }
    if let Some(Value::Array(variants)) = object.get("enum") {
        return variants
            .first()
            .cloned()
            .ok_or_else(|| SynthesisError::UnsupportedSchema { at: path.to_string() });
    }

    let type_name = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| SynthesisError::UnsupportedSchema { at: path.to_string() })?;

    match type_name {
        "object" => synthesize_object(object, bindings, path, fixture_bound_properties),
        "array" => synthesize_array(object, bindings, path, fixture_bound_properties),
        "string" => Ok(Value::String(synthesize_string(object))),
        "integer" => Ok(Value::Number(synthesize_integer(object))),
        "number" => Ok(Value::Number(synthesize_number(object))),
        "boolean" => Ok(Value::Bool(true)),
        "null" => Ok(Value::Null),
        _ => Err(SynthesisError::UnsupportedSchema { at: path.to_string() }),
    }
}

fn synthesize_object(
    object: &Map<String, Value>,
    bindings: &FixtureBindings,
    path: &str,
    fixture_bound_properties: &mut Vec<String>,
) -> Result<Value, SynthesisError> {
    let Some(properties) = object.get("properties").and_then(Value::as_object) else {
        // An object schema with no `properties` at all is valid and synthesises to `{}` —
        // there is nothing declared to generate.
        return Ok(Value::Object(Map::new()));
    };
    let required: Vec<&str> = object
        .get("required")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let mut out = Map::new();
    for (name, subschema) in properties {
        // Every declared property is synthesised, not just the required ones: a tool that
        // treats an absent optional argument differently from an explicitly-provided one is
        // exactly the kind of behaviour this harness wants a chance to observe, not one this
        // generator should foreclose by omitting optional fields.
        let child_path = if path.is_empty() { name.clone() } else { format!("{path}.{name}") };

        if let Some(bound) = bindings.0.get(name) {
            out.insert(name.clone(), bound.clone());
            fixture_bound_properties.push(child_path);
            continue;
        }
        let value = synthesize_at(subschema, bindings, &child_path, fixture_bound_properties)?;
        out.insert(name.clone(), value);
    }

    // `required` is enforced as a postcondition, not just a generation hint: every name it
    // lists must actually have ended up in `out`, whether via a binding or synthesis above.
    // A schema that requires a property absent from `properties` is a shape this generator
    // does not understand well enough to synthesise correctly.
    for name in required {
        if !out.contains_key(name) {
            return Err(SynthesisError::UnsupportedSchema {
                at: if path.is_empty() { name.to_string() } else { format!("{path}.{name}") },
            });
        }
    }

    Ok(Value::Object(out))
}

fn synthesize_array(
    object: &Map<String, Value>,
    bindings: &FixtureBindings,
    path: &str,
    fixture_bound_properties: &mut Vec<String>,
) -> Result<Value, SynthesisError> {
    let min_items = object.get("minItems").and_then(Value::as_u64).unwrap_or(1);
    let Some(items_schema) = object.get("items") else {
        // No `items` schema at all: synthesise the minimum-length array of `null`s rather
        // than fail outright — an untyped array is still a structurally valid JSON array.
        return Ok(Value::Array(vec![Value::Null; min_items as usize]));
    };
    let item_path = format!("{path}[]");
    let mut items = Vec::new();
    for _ in 0..min_items {
        items.push(synthesize_at(items_schema, bindings, &item_path, fixture_bound_properties)?);
    }
    Ok(Value::Array(items))
}

fn synthesize_string(object: &Map<String, Value>) -> String {
    let min_length = object.get("minLength").and_then(Value::as_u64).unwrap_or(0) as usize;
    let mut s = String::from("example");
    while s.len() < min_length {
        s.push('x');
    }
    s
}

fn synthesize_integer(object: &Map<String, Value>) -> Number {
    let minimum = object.get("minimum").and_then(Value::as_i64);
    let maximum = object.get("maximum").and_then(Value::as_i64);
    let mut value = minimum.unwrap_or(0);
    if let Some(max) = maximum {
        value = value.min(max);
    }
    Number::from(value)
}

fn synthesize_number(object: &Map<String, Value>) -> Number {
    let minimum = object.get("minimum").and_then(Value::as_f64);
    let maximum = object.get("maximum").and_then(Value::as_f64);
    let mut value = minimum.unwrap_or(0.0);
    if let Some(max) = maximum {
        value = value.min(max);
    }
    Number::from_f64(value).unwrap_or_else(|| Number::from(0))
}

/// One argument value that must be reused, byte-for-byte, across every arm named in
/// `reused_for` — architecture.md §4.2's noise floor (`N = D1 Δ D1'`) is only meaningful if
/// `D1` and `D1'` genuinely call with identical arguments; two independently-synthesised
/// values that merely happen to be equal today would satisfy that by coincidence, not by
/// guarantee. Produced by [`reuse_across_arms`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReusedArguments {
    /// The single argument value every arm in `reused_for` must call with.
    pub arguments: Value,
    /// Which arms this exact value is required to be identical across.
    pub reused_for: Vec<String>,
}

/// Tag `arguments` as required to be identical across every arm in `arm_ids`. Calling this
/// once and handing the same [`ReusedArguments::arguments`] to every arm named in
/// `reused_for` is how a caller makes cross-arm argument reuse an explicit, inspectable fact
/// — this crate's own "must not" — instead of an accident of two separate `synthesize` calls
/// happening to agree.
#[must_use]
pub const fn reuse_across_arms(arguments: Value, arm_ids: Vec<String>) -> ReusedArguments {
    ReusedArguments { arguments, reused_for: arm_ids }
}

/// Produce a cache-busting variant of `arguments`: every string leaf gets a fixed suffix
/// appended and every numeric leaf is incremented by one; `bool` and `null` leaves are left
/// untouched (perturbing a boolean flips its meaning entirely rather than merely busting a
/// cache key, and there is no meaningful direction to perturb `null` in).
///
/// Deterministic, not random: calling this twice on the same input produces the same output,
/// so a caller needing the *same* busted variant again (rather than a fresh one) gets it just
/// by calling this again, with no state to thread through.
///
/// This is a primitive, not a protocol: *whether and when* a cache-busting variant gets used
/// is P2-09's multi-arm protocol's decision, once it exists. This function only guarantees
/// the variant it hands back is both different from the input and reproducible.
#[must_use]
pub fn cache_busting_variant(arguments: &Value) -> Value {
    match arguments {
        Value::String(s) => Value::String(format!("{s}__cachebust")),
        Value::Number(n) => n
            .as_i64()
            .map(|i| Value::Number(Number::from(i + 1)))
            .or_else(|| n.as_f64().and_then(|f| Number::from_f64(f + 1.0)).map(Value::Number))
            .unwrap_or_else(|| Value::Number(n.clone())),
        Value::Array(items) => Value::Array(items.iter().map(cache_busting_variant).collect()),
        Value::Object(map) => {
            Value::Object(map.iter().map(|(k, v)| (k.clone(), cache_busting_variant(v))).collect())
        }
        Value::Bool(_) | Value::Null => arguments.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn synthesizes_every_declared_property_type() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "count": { "type": "integer", "minimum": 5 },
                "ratio": { "type": "number", "minimum": 0.5 },
                "active": { "type": "boolean" },
                "tags": { "type": "array", "items": { "type": "string" } },
            },
            "required": ["name", "count"],
        });
        let result = synthesize(&schema, &FixtureBindings::new()).expect("synthesize");
        let args = result.arguments.as_object().expect("object");
        assert_eq!(args["name"], json!("example"));
        assert_eq!(args["count"], json!(5));
        assert_eq!(args["ratio"], json!(0.5));
        assert_eq!(args["active"], json!(true));
        assert_eq!(args["tags"], json!(["example"]));
        assert!(result.fixture_bound_properties.is_empty());
    }

    #[test]
    fn enum_and_const_pick_a_declared_value_rather_than_synthesising_one() {
        let schema = json!({
            "type": "object",
            "properties": {
                "mode": { "enum": ["fast", "slow"] },
                "version": { "const": 3 },
            },
            "required": ["mode", "version"],
        });
        let result = synthesize(&schema, &FixtureBindings::new()).expect("synthesize");
        assert_eq!(result.arguments["mode"], json!("fast"));
        assert_eq!(result.arguments["version"], json!(3));
    }

    #[test]
    fn min_length_and_min_items_are_honoured() {
        let schema = json!({
            "type": "object",
            "properties": {
                "code": { "type": "string", "minLength": 10 },
                "items": { "type": "array", "items": { "type": "integer" }, "minItems": 3 },
            },
            "required": [],
        });
        let result = synthesize(&schema, &FixtureBindings::new()).expect("synthesize");
        assert!(result.arguments["code"].as_str().unwrap().len() >= 10);
        assert_eq!(result.arguments["items"].as_array().unwrap().len(), 3);
    }

    /// The exit criterion's "semantic validity via fixture binding to entities that actually
    /// exist," made concrete: the bound value is a path P2-05's own generic fixture
    /// genuinely seeds (`world::SEEDED_DATABASE_PATH`), not a coincidentally matching
    /// literal — if that constant ever changes, this test changes with it rather than
    /// silently testing against a stale path.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_bound_property_uses_the_real_fixture_entity_and_is_recorded_as_bound() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "unrelated": { "type": "integer" },
            },
            "required": ["path"],
        });
        let bindings = FixtureBindings::new().with("path", json!(world::SEEDED_DATABASE_PATH));
        let result = synthesize(&schema, &bindings).expect("synthesize");
        assert_eq!(result.arguments["path"], json!(world::SEEDED_DATABASE_PATH));
        assert_eq!(result.fixture_bound_properties, vec!["path".to_string()]);
        // `unrelated` was never bound, so it must still be structurally synthesised.
        assert_eq!(result.arguments["unrelated"], json!(0));
    }

    #[test]
    fn a_required_property_missing_from_the_schema_is_an_unsupported_schema_error() {
        let schema = json!({
            "type": "object",
            "properties": { "a": { "type": "string" } },
            "required": ["a", "b"],
        });
        let err = synthesize(&schema, &FixtureBindings::new()).expect_err("must fail");
        assert!(matches!(err, SynthesisError::UnsupportedSchema { .. }));
    }

    #[test]
    fn an_unrecognised_schema_shape_is_rejected_rather_than_guessed_at() {
        let schema = json!({ "oneOf": [{ "type": "string" }, { "type": "integer" }] });
        let err = synthesize(&schema, &FixtureBindings::new()).expect_err("must fail");
        assert!(matches!(err, SynthesisError::UnsupportedSchema { at } if at.is_empty()));
    }

    #[test]
    fn synthesis_is_deterministic_across_repeated_calls() {
        let schema = json!({
            "type": "object",
            "properties": { "n": { "type": "string" } },
            "required": ["n"],
        });
        let a = synthesize(&schema, &FixtureBindings::new()).expect("a");
        let b = synthesize(&schema, &FixtureBindings::new()).expect("b");
        assert_eq!(a.arguments, b.arguments, "the noise-floor pair D1/D1' relies on this");
    }

    #[test]
    fn reuse_across_arms_records_the_exact_same_value_for_every_tagged_arm() {
        let args = json!({ "n": "example" });
        let reused = reuse_across_arms(args.clone(), vec!["arm-1".into(), "arm-1-prime".into()]);
        assert_eq!(reused.arguments, args);
        assert_eq!(reused.reused_for, vec!["arm-1".to_string(), "arm-1-prime".to_string()]);
    }

    #[test]
    fn cache_busting_variant_differs_from_the_original_but_is_reproducible() {
        let original = json!({ "name": "example", "count": 5, "flag": true, "nested": { "n": 1 } });
        let busted_once = cache_busting_variant(&original);
        let busted_again = cache_busting_variant(&original);
        assert_ne!(busted_once, original, "a cache-busting variant must actually differ");
        assert_eq!(busted_once, busted_again, "busting the same input twice must be deterministic");
        assert_eq!(busted_once["name"], json!("example__cachebust"));
        assert_eq!(busted_once["count"], json!(6));
        assert_eq!(busted_once["flag"], json!(true), "booleans are left untouched");
        assert_eq!(busted_once["nested"]["n"], json!(2));
    }
}
