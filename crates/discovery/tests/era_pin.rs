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

//!
//! P0-13 extends the same claim across a paginated walk: the page framing (`nextCursor`, and
//! each page's own `resultType`/`ttlMs`/`cacheScope`) must stay outside the preimage too, so
//! a dual-era server pins identically whichever lifecycle *and* whichever page split
//! produced its tool list. See ADR-015.

use discovery::{pin_pages, pin_tools};

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

/// P0-13's version of the first test: the same two tools delivered as **one legacy page**,
/// as **two modern pages with a cursor between them**, and as two legacy pages, all pin
/// identically.
///
/// This is the composition that matters in practice and the one that would be easiest to get
/// wrong. A modern paginated walk differs from a single legacy response in three independent
/// ways at once — the per-page required result fields, the `nextCursor` member, and the page
/// boundary — and all three must be outside the preimage or every dual-era server with more
/// than one page of tools looks rug-pulled the day it changes either.
#[test]
fn a_pin_survives_both_the_revision_and_the_page_split_at_once() {
    let first = r#"{"name":"read_file","description":"Reads a file","inputSchema":{"type":"object","properties":{"path":{"type":"string"}}},"annotations":{"readOnlyHint":true}}"#;
    let second = r#"{"name":"write_file","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":false,"destructiveHint":true}}"#;

    let legacy_one_page =
        vec![format!(r#"{{"jsonrpc":"2.0","id":1,"result":{{"tools":{TOOLS}}}}}"#).into_bytes()];
    let legacy_two_pages = vec![
        format!(r#"{{"jsonrpc":"2.0","id":1,"result":{{"tools":[{first}],"nextCursor":"p2"}}}}"#)
            .into_bytes(),
        format!(r#"{{"jsonrpc":"2.0","id":2,"result":{{"tools":[{second}]}}}}"#).into_bytes(),
    ];
    let modern_two_pages = vec![
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"result":{{"resultType":"complete","ttlMs":3600000,"cacheScope":"public","tools":[{first}],"nextCursor":"opaque-cursor-value"}}}}"#
        )
        .into_bytes(),
        format!(
            r#"{{"jsonrpc":"2.0","id":2,"result":{{"resultType":"complete","ttlMs":0,"cacheScope":"private","tools":[{second}]}}}}"#
        )
        .into_bytes(),
    ];

    let (baseline_tools, baseline_server) = pin_pages(&legacy_one_page).expect("pins");
    assert_eq!(baseline_tools.len(), 2);
    // Same bytes, through the single-page entry point: the two must agree, or `pin_tools`
    // and `pin_pages` have diverged.
    assert_eq!(pin_tools(&legacy_one_page[0]).expect("pins").1, baseline_server);

    for (label, walk) in
        [("two legacy pages", legacy_two_pages), ("two modern pages", modern_two_pages)]
    {
        let (tools, server) = pin_pages(&walk).expect("pins");
        assert_eq!(
            server, baseline_server,
            "{label}: neither the revision's result fields nor the page framing may enter \
             the preimage"
        );
        assert_eq!(
            tools.iter().map(|t| t.pin).collect::<Vec<_>>(),
            baseline_tools.iter().map(|t| t.pin).collect::<Vec<_>>(),
            "{label}: per-tool pins must match too"
        );
    }
}

/// The complement for the paginated case, so the test above cannot pass by the pin having
/// gone insensitive to page content: an annotation flipped on the tool that lives on page
/// **two** of a modern walk still moves the pin.
#[test]
fn a_change_on_a_later_modern_page_still_moves_the_pin() {
    let page_one = br#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","ttlMs":0,"cacheScope":"private","tools":[{"name":"read_file","inputSchema":{"type":"object"}}],"nextCursor":"p2"}}"#
        .to_vec();
    let page_two = |annotations: &str| {
        format!(
            r#"{{"jsonrpc":"2.0","id":2,"result":{{"resultType":"complete","ttlMs":0,"cacheScope":"private","tools":[{{"name":"write_file","inputSchema":{{"type":"object"}},"annotations":{annotations}}}]}}}}"#
        )
        .into_bytes()
    };

    let honest = vec![page_one.clone(), page_two(r#"{"destructiveHint":true}"#)];
    let flipped = vec![page_one, page_two(r#"{"destructiveHint":false}"#)];
    assert_ne!(
        pin_pages(&honest).expect("pins").1,
        pin_pages(&flipped).expect("pins").1,
        "an annotation change on page two must still change the pin — that is the rug pull \
         the pin exists to catch, and pagination must not create a blind spot for it"
    );
}
