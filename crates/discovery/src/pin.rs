//! Metadata pinning (P0-02): a hash over `(name, inputSchema, annotations, description)`
//! *as received*.
//!
//! Defends against rug-pull attacks that mutate a tool's declared metadata after a client
//! has already approved it based on an earlier snapshot (architecture.md §0 — *"a verdict
//! without a pin is meaningless"*).
//!
//! **Must not:** normalise before hashing. The pin is over bytes, not semantics: reorder
//! the JSON keys in a tool's schema and the pin must change. This module never routes a
//! captured field through `serde_json::Value` — that type re-serialises through a
//! `BTreeMap` and would silently sort keys back into a canonical order, making the pin
//! insensitive to the exact byte-reordering attack it exists to catch. Every field is kept
//! as a [`serde_json::value::RawValue`], which borrows its untouched source span instead.

use datamodel::Digest;
use serde::Deserialize;
use serde_json::value::RawValue;
use sha2::{Digest as _, Sha256};

/// One tool's pin, alongside its name for reporting — `TOOL_SNAPSHOT.metadata_pin` and
/// `TOOL_SNAPSHOT.tool_name` (architecture.md §6) are both populated from this.
#[derive(Debug, Clone)]
pub struct ToolPin {
    /// The tool's name, decoded from the raw JSON string (for reporting only — it plays
    /// no role in the pin's preimage beyond its own raw bytes).
    pub name: String,
    /// The pin: `sha256(sha256(name) || sha256(inputSchema) || sha256(annotations) ||
    /// sha256(description))`, each inner hash over the exact bytes received (or a
    /// presence marker if the field was absent).
    pub pin: Digest,
}

/// Why pinning failed. Always a shape problem with the `tools/list` response itself —
/// pinning runs on bytes P0-01 already captured successfully, so a failure here means the
/// server's response wasn't the shape the MCP spec promises.
#[derive(Debug)]
pub enum PinError {
    /// The captured bytes are not valid UTF-8 (JSON is required to be, per RFC 8259).
    NotUtf8(std::str::Utf8Error),
    /// The bytes are valid UTF-8 but not the expected JSON-RPC / `tools/list` shape.
    Malformed(String),
}

impl std::fmt::Display for PinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotUtf8(e) => write!(f, "tools/list response is not valid UTF-8: {e}"),
            Self::Malformed(msg) => write!(f, "tools/list response is malformed: {msg}"),
        }
    }
}

impl std::error::Error for PinError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotUtf8(e) => Some(e),
            Self::Malformed(_) => None,
        }
    }
}

/// Pin every tool in a raw `tools/list` response, plus a server-level pin over the whole
/// tool set.
///
/// The server-level pin is order-sensitive: the tools array is hashed in the order the
/// server returned it, on the same "bytes, not semantics" principle as the per-tool pin — a
/// server that reorders its tool list between two discoveries has changed its response,
/// and that is exactly the kind of change a pin exists to make visible rather than absorb.
///
/// # Errors
///
/// Returns [`PinError::NotUtf8`] if `tools_list_raw` is not valid UTF-8, or
/// [`PinError::Malformed`] if it doesn't parse as a JSON-RPC `tools/list` response.
pub fn pin_tools(tools_list_raw: &[u8]) -> Result<(Vec<ToolPin>, Digest), PinError> {
    let text = std::str::from_utf8(tools_list_raw).map_err(PinError::NotUtf8)?;

    #[derive(Deserialize)]
    struct Envelope<'a> {
        #[serde(borrow)]
        result: &'a RawValue,
    }
    #[derive(Deserialize)]
    struct ToolsResult<'a> {
        #[serde(borrow)]
        tools: Vec<&'a RawValue>,
    }

    let envelope: Envelope = serde_json::from_str(text)
        .map_err(|e| PinError::Malformed(format!("not a JSON-RPC response: {e}")))?;
    let tools_result: ToolsResult = serde_json::from_str(envelope.result.get())
        .map_err(|e| PinError::Malformed(format!("result is not a tools/list shape: {e}")))?;

    let mut tool_pins = Vec::with_capacity(tools_result.tools.len());
    let mut server_preimage = Vec::with_capacity(tools_result.tools.len() * 32);
    for raw_tool in tools_result.tools {
        let tool_pin = pin_one_tool(raw_tool)?;
        server_preimage.extend_from_slice(tool_pin.pin.as_bytes());
        tool_pins.push(tool_pin);
    }

    Ok((tool_pins, digest_of(&server_preimage)))
}

fn pin_one_tool(raw_tool: &RawValue) -> Result<ToolPin, PinError> {
    #[derive(Deserialize)]
    struct ToolFields<'a> {
        #[serde(borrow)]
        name: &'a RawValue,
        #[serde(borrow, rename = "inputSchema")]
        input_schema: &'a RawValue,
        #[serde(borrow, default)]
        annotations: Option<&'a RawValue>,
        #[serde(borrow, default)]
        description: Option<&'a RawValue>,
    }

    let fields: ToolFields = serde_json::from_str(raw_tool.get())
        .map_err(|e| PinError::Malformed(format!("tool entry is not the expected shape: {e}")))?;

    let name: String = serde_json::from_str(fields.name.get())
        .map_err(|e| PinError::Malformed(format!("tool name is not a JSON string: {e}")))?;

    let mut preimage = Vec::with_capacity(4 * 32);
    preimage.extend_from_slice(field_digest(Some(fields.name)).as_bytes());
    preimage.extend_from_slice(field_digest(Some(fields.input_schema)).as_bytes());
    preimage.extend_from_slice(field_digest(fields.annotations).as_bytes());
    preimage.extend_from_slice(field_digest(fields.description).as_bytes());

    Ok(ToolPin { name, pin: digest_of(&preimage) })
}

/// Digest one field's raw bytes exactly as received, with a leading presence marker so "the
/// key was absent" and "the key was present with these exact bytes" can never collide —
/// distinct from `sha256(<empty>)`, which is what a present-but-empty value would hash to
/// without it.
///
/// Caveat: this only distinguishes presence at the level `Option<&RawValue>` can see. Serde's
/// generic `Option<T>` deserialisation treats a literal JSON `null` the same as a missing
/// key — the optionality check happens before `RawValue` ever captures anything — so this
/// module cannot and does not try to distinguish `"field": null` from omitting `field`
/// entirely. Both pin identically. See the tests below for the exact boundary.
fn field_digest(raw: Option<&RawValue>) -> Digest {
    let mut preimage = Vec::new();
    match raw {
        Some(v) => {
            preimage.push(1u8);
            preimage.extend_from_slice(v.get().as_bytes());
        }
        None => preimage.push(0u8),
    }
    digest_of(&preimage)
}

fn digest_of(bytes: &[u8]) -> Digest {
    Digest::from_bytes(Sha256::digest(bytes).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools_list(tools_json: &str) -> Vec<u8> {
        format!(r#"{{"jsonrpc":"2.0","id":1,"result":{{"tools":[{tools_json}]}}}}"#).into_bytes()
    }

    #[test]
    fn identical_bytes_produce_identical_pins() {
        let raw = tools_list(
            r#"{"name":"read_file","inputSchema":{"type":"object"},"description":"reads a file"}"#,
        );
        let (pins_a, server_a) = pin_tools(&raw).expect("pin");
        let (pins_b, server_b) = pin_tools(&raw).expect("pin");
        assert_eq!(pins_a[0].pin, pins_b[0].pin);
        assert_eq!(server_a, server_b);
    }

    /// The exit criterion, verbatim: "Test: reorder JSON keys → pin changes. That is
    /// correct behaviour, not a bug."
    #[test]
    fn reordering_json_keys_changes_the_pin() {
        let original = tools_list(
            r#"{"name":"read_file","inputSchema":{"type":"object","properties":{"a":1,"b":2}}}"#,
        );
        let reordered = tools_list(
            r#"{"inputSchema":{"properties":{"b":2,"a":1},"type":"object"},"name":"read_file"}"#,
        );

        let (pins_original, server_original) = pin_tools(&original).expect("pin");
        let (pins_reordered, server_reordered) = pin_tools(&reordered).expect("pin");

        assert_ne!(
            pins_original[0].pin, pins_reordered[0].pin,
            "reordering keys must change the pin even though the two documents are \
             semantically equivalent JSON"
        );
        assert_ne!(server_original, server_reordered);
    }

    #[test]
    fn different_tool_content_produces_different_pins() {
        let a = tools_list(r#"{"name":"read_file","inputSchema":{"type":"object"}}"#);
        let b = tools_list(r#"{"name":"write_file","inputSchema":{"type":"object"}}"#);
        let (pins_a, _) = pin_tools(&a).expect("pin");
        let (pins_b, _) = pin_tools(&b).expect("pin");
        assert_ne!(pins_a[0].pin, pins_b[0].pin);
    }

    /// Documents a real boundary rather than asserting a false one: serde's `Option<T>`
    /// collapses a literal JSON `null` and an absent key before `RawValue` ever sees
    /// either, so this module cannot distinguish them. See `field_digest`'s doc comment.
    #[test]
    fn explicit_null_and_an_absent_key_pin_identically() {
        let with_null = tools_list(r#"{"name":"t","inputSchema":{},"description":null}"#);
        let without_key = tools_list(r#"{"name":"t","inputSchema":{}}"#);
        let (pins_null, _) = pin_tools(&with_null).expect("pin");
        let (pins_absent, _) = pin_tools(&without_key).expect("pin");
        assert_eq!(pins_null[0].pin, pins_absent[0].pin);
    }

    #[test]
    fn a_present_optional_field_pins_differently_from_an_absent_one() {
        let with_description =
            tools_list(r#"{"name":"t","inputSchema":{},"description":"hello"}"#);
        let without_key = tools_list(r#"{"name":"t","inputSchema":{}}"#);
        let (pins_present, _) = pin_tools(&with_description).expect("pin");
        let (pins_absent, _) = pin_tools(&without_key).expect("pin");
        assert_ne!(pins_present[0].pin, pins_absent[0].pin);
    }

    #[test]
    fn server_pin_is_sensitive_to_tool_order() {
        let forward = tools_list(
            r#"{"name":"a","inputSchema":{}},{"name":"b","inputSchema":{}}"#,
        );
        let reversed = tools_list(
            r#"{"name":"b","inputSchema":{}},{"name":"a","inputSchema":{}}"#,
        );
        let (_, server_forward) = pin_tools(&forward).expect("pin");
        let (_, server_reversed) = pin_tools(&reversed).expect("pin");
        assert_ne!(server_forward, server_reversed);
    }

    #[test]
    fn tool_name_is_recovered_for_reporting() {
        let raw = tools_list(r#"{"name":"read_file","inputSchema":{}}"#);
        let (pins, _) = pin_tools(&raw).expect("pin");
        assert_eq!(pins[0].name, "read_file");
    }

    #[test]
    fn malformed_response_is_rejected_not_guessed_at() {
        let err = pin_tools(b"not json at all").expect_err("must reject");
        assert!(matches!(err, PinError::Malformed(_)));
    }
}
