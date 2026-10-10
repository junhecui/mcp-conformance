//! P0-02's pin must be **revision-independent**, and P0-11 is the first change that could
//! have broken that.
//!
//! `2026-07-28` adds three required fields to every result (`resultType`, `ttlMs`,
//! `cacheScope`), so a `tools/list` response from a modern server is not byte-identical to a
//! legacy one even when it declares exactly the same tools. The per-tool pin hashes only
//! `(name, inputSchema, annotations, description)` and the server pin hashes the per-tool
//! pins, so the new fields sit outside both preimages. That is a property of `pin_tools`
//! rather than an accident of the current test fixtures, and this file is the check —
//! `pin_tools` itself needed no change for P0-11, which is the claim being made here.

use discovery::pin_tools;

/// The same tools array, inside the two result shapes.
const TOOLS: &str = r#"[{"name":"read_file","description":"Reads a file","inputSchema":{"type":"object","properties":{"path":{"type":"string"}}},"annotations":{"readOnlyHint":true}},{"name":"write_file","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":false,"destructiveHint":true}}]"#;

#[test]
fn a_tool_pin_is_identical_across_the_legacy_and_modern_result_shapes() {
    let legacy = format!(r#"{{"jsonrpc":"2.0","id":1,"result":{{"tools":{TOOLS}}}}}"#);
    let modern = format!(
        r#"{{"jsonrpc":"2.0","id":1,"result":{{"resultType":"complete","ttlMs":3600000,"cacheScope":"public","tools":{TOOLS},"_meta":{{"io.modelcontextprotocol/serverInfo":{{"name":"s","version":"1"}}}}}}}}"#
    );
    assert_ne!(legacy, modern, "the two responses must genuinely differ byte-for-byte");

    let (legacy_tools, legacy_server) = pin_tools(legacy.as_bytes()).expect("legacy pins");
    let (modern_tools, modern_server) = pin_tools(modern.as_bytes()).expect("modern pins");

    assert_eq!(legacy_tools.len(), 2);
    assert_eq!(
        legacy_server, modern_server,
        "the server pin must not move when a revision adds result-level fields — otherwise \
         every server in the corpus would look rug-pulled the day it upgraded its SDK"
    );
    for (a, b) in legacy_tools.iter().zip(modern_tools.iter()) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.pin, b.pin, "per-tool pin for {} must be revision-independent", a.name);
    }
}

/// The complement, so the test above cannot pass by the pin having gone insensitive: a
/// change *inside* the pinned tuple still moves the pin, in the modern shape as in the
/// legacy one.
#[test]
fn a_change_inside_the_pinned_tuple_still_moves_the_pin_in_the_modern_shape() {
    let modern = |annotations: &str| {
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"result":{{"resultType":"complete","ttlMs":0,"cacheScope":"private","tools":[{{"name":"read_file","description":"Reads a file","inputSchema":{{"type":"object"}},"annotations":{annotations}}}]}}}}"#
        )
    };
    let (honest, _) = pin_tools(modern(r#"{"readOnlyHint":true}"#).as_bytes()).expect("pins");
    let (flipped, _) = pin_tools(modern(r#"{"readOnlyHint":false}"#).as_bytes()).expect("pins");
    assert_ne!(
        honest[0].pin, flipped[0].pin,
        "an annotation change must still change the pin — that is the rug pull the pin exists \
         to catch"
    );
}
