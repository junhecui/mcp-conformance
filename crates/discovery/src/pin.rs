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
/// The single-page entry point, kept because most servers return one page and because every
/// caller that genuinely has one response should say so. It is [`pin_pages`] over a
/// one-element slice — *the same code*, not an equivalent one — so the single-page pin is
/// byte-identical to what this function produced before P0-13, and every pin already
/// published stays comparable.
pub fn pin_tools(tools_list_raw: &[u8]) -> Result<(Vec<ToolPin>, Digest), PinError> {
    pin_pages(&[tools_list_raw])
}

/// Pin every tool across a whole paginated `tools/list` walk, plus a server-level pin over
/// the tool set the walk observed (P0-13).
///
/// # The preimage is the per-tool pins, and page framing is not in it
///
/// `sha256(tool_pin_0 || tool_pin_1 || … || tool_pin_n)`, over every tool of every page in
/// walk order. Page framing — `nextCursor`, the per-page `resultType`/`ttlMs`/`cacheScope`
/// of a modern result, the page boundaries themselves — is **absent** from the preimage,
/// and that absence is the decision (ADR-015).
///
/// Two properties follow, and P0-13 proves both in both directions:
///
/// - **The same tools split across pages differently pin identically.** One page of six
///   tools, two pages of three, and six pages of one all produce the same server pin. A
///   server that changes its page size — or a gateway that changes it for the server — must
///   not look rug-pulled, because nothing about the tools it declares changed.
/// - **A change inside any tool's `(name, inputSchema, annotations, description)` still
///   moves the pin**, wherever on the walk that tool sits. The per-tool preimage is
///   untouched by this change, so the rug-pull detection P0-02 exists for is unweakened.
///
/// The alternative — hashing a canonical concatenation of the page *bytes* — fails the
/// first property outright (page framing differs the moment the split does) and would also
/// break the revision-independence P0-11 proved, since a modern page carries three required
/// result fields a legacy page does not.
///
/// Order-sensitivity is preserved exactly as before: the tools are hashed in the order the
/// walk observed them, across pages as well as within one, so a server that reorders its
/// tool list changes its pin. That is the "bytes, not semantics" principle applied to a
/// sequence rather than to a byte range.
///
/// # Errors
///
/// [`PinError::Malformed`] if `pages` is empty — a pin over no response at all would be a
/// valid-looking digest of nothing — or if any page is not a `tools/list` shape, and
/// [`PinError::NotUtf8`] if any page is not valid UTF-8. A walk is pinned whole or not at
/// all: a partial pin over the pages that happened to parse would attest to a tool set no
/// server ever returned.
///
/// # A walk of zero tools
///
/// A walk whose pages declare no tools at all pins to `sha256("")`, however many pages it
/// spanned. That is deliberate, not the "digest of nothing" the empty-page-list refusal
/// exists for: an empty page *list* means no server answered, whereas a page with an empty
/// `tools` array is a server declaring an empty tool set — and every server declaring the
/// same (empty) set pinning identically, regardless of page split, is precisely the
/// split-stability property above. A tool added later still moves it.
/// `a_zero_tool_walk_pins_to_the_empty_preimage_whatever_its_page_count` pins the decision.
pub fn pin_pages<P: AsRef<[u8]>>(pages: &[P]) -> Result<(Vec<ToolPin>, Digest), PinError> {
    if pages.is_empty() {
        return Err(PinError::Malformed(
            "no tools/list pages to pin — a pin over no response is not a pin".into(),
        ));
    }
    let mut tool_pins = Vec::new();
    for page in pages {
        tool_pins.extend(pin_page(page.as_ref())?);
    }
    let server_pin = server_pin_of(&tool_pins);
    Ok((tool_pins, server_pin))
}

/// Pin the tools of **one** page of a walk, in page order — the per-page half of
/// [`pin_pages`], exposed so a consumer can stream a long walk one page at a time instead of
/// holding every page resident.
///
/// [`pin_pages`] is literally "concatenate `pin_page` over the walk, then
/// [`server_pin_of`]", so a streaming caller that does the same reproduces its server pin
/// exactly. The caller owns the obligations [`pin_pages`] discharges for it: refuse an empty
/// walk, and treat one failed page as failing the whole walk.
///
/// # Errors
///
/// As [`pin_pages`], for this one page.
pub fn pin_page(page: &[u8]) -> Result<Vec<ToolPin>, PinError> {
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

    let text = std::str::from_utf8(page).map_err(PinError::NotUtf8)?;
    let envelope: Envelope = serde_json::from_str(text)
        .map_err(|e| PinError::Malformed(format!("not a JSON-RPC response: {e}")))?;
    let tools_result: ToolsResult = serde_json::from_str(envelope.result.get())
        .map_err(|e| PinError::Malformed(format!("result is not a tools/list shape: {e}")))?;
    tools_result.tools.into_iter().map(pin_one_tool).collect()
}

/// The server pin over a walk's per-tool pins, in walk order:
/// `sha256(tool_pin_0 || … || tool_pin_n)`. Order-sensitive by design (P0-02).
#[must_use]
pub fn server_pin_of(tool_pins: &[ToolPin]) -> Digest {
    let mut preimage = Vec::with_capacity(tool_pins.len() * 32);
    for tool_pin in tool_pins {
        preimage.extend_from_slice(tool_pin.pin.as_bytes());
    }
    digest_of(&preimage)
}

/// A **secondary**, order-insensitive pin over the same per-tool pins: the multiset, hashed
/// in sorted order. Published *beside* the server pin, never instead of it.
///
/// Why it exists (P0-13 review). The primary pin is order-sensitive on purpose, and
/// ADR-015 tested its split-stability only with the tools in one global order. Across pages
/// order is much more plausibly an artifact — a sharded backend or a gateway serving pages
/// in a different sequence — than a declaration, and four tools served as the same two
/// pages in the other order moved the primary pin while the per-tool pins were identical.
/// A pin that moves for that is a false-positive rug pull, the failure ADR-015 calls worse
/// than no pin because it trains the reader to ignore it. Sorting the primary was rejected
/// (it discards P0-02's order sensitivity and every published pin), so instead a reader gets
/// both: primary moved and secondary did not means **reordered**; both moved means
/// **changed**.
///
/// Domain-separated with a fixed prefix so it can never collide with a primary pin over the
/// same tools that happen already to be in sorted order.
#[must_use]
pub fn unordered_server_pin_of(tool_pins: &[ToolPin]) -> Digest {
    let mut pins: Vec<&[u8]> = tool_pins.iter().map(|t| t.pin.as_bytes().as_slice()).collect();
    pins.sort_unstable();
    let mut preimage = Vec::with_capacity(UNORDERED_PIN_DOMAIN.len() + pins.len() * 32);
    preimage.extend_from_slice(UNORDERED_PIN_DOMAIN);
    for pin in pins {
        preimage.extend_from_slice(pin);
    }
    digest_of(&preimage)
}

/// Domain-separation prefix for [`unordered_server_pin_of`].
const UNORDERED_PIN_DOMAIN: &[u8] = b"mcp-conformance/unordered-server-pin/v1\0";

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

    // ---- P0-13: pagination (ADR-015) ----

    fn pages(page_bodies: &[&str]) -> Vec<Vec<u8>> {
        page_bodies.iter().map(|tools| tools_list(tools)).collect()
    }

    /// Three tools, declared as one page, as three pages, and as two uneven pages. All three
    /// walks must pin identically, per-tool and server-wide.
    ///
    /// This is ADR-015's forward direction and the reason the preimage is the per-tool pins
    /// rather than the page bytes: a server that changes its page size, or a gateway that
    /// changes it for the server, must not look rug-pulled. Nothing about the tools changed.
    #[test]
    fn the_same_tools_split_across_pages_differently_pin_identically() {
        let a = r#"{"name":"a","inputSchema":{"type":"object"},"description":"A"}"#;
        let b = r#"{"name":"b","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":true}}"#;
        let c = r#"{"name":"c","inputSchema":{}}"#;

        let one_page = pages(&[&format!("{a},{b},{c}")]);
        let three_pages = pages(&[a, b, c]);
        let two_uneven = pages(&[&format!("{a},{b}"), c]);
        // A page declaring no tools at all is still a page, and must not perturb the pin.
        let with_an_empty_page = pages(&[a, "", &format!("{b},{c}")]);

        let (tools_one, server_one) = pin_pages(&one_page).expect("pin");
        assert_eq!(tools_one.len(), 3);

        for (label, walk) in [
            ("three pages", three_pages),
            ("two uneven pages", two_uneven),
            ("a page with no tools in the middle", with_an_empty_page),
        ] {
            let (tools, server) = pin_pages(&walk).expect("pin");
            assert_eq!(
                server, server_one,
                "{label}: the same tools split differently must pin identically"
            );
            assert_eq!(
                tools.iter().map(|t| t.pin).collect::<Vec<_>>(),
                tools_one.iter().map(|t| t.pin).collect::<Vec<_>>(),
                "{label}: per-tool pins must match too, not just the server pin"
            );
        }
    }

    /// The other direction, which is what stops the test above passing by the pin having
    /// gone insensitive: change one byte inside a tool that lives on page **two** and the
    /// server pin must move.
    #[test]
    fn a_change_inside_a_tool_on_a_later_page_still_moves_the_pin() {
        let first = r#"{"name":"a","inputSchema":{"type":"object"}}"#;
        let before = pages(&[
            first,
            r#"{"name":"b","inputSchema":{"type":"object","properties":{"p":2,"q":1}}}"#,
        ]);

        for (label, changed) in [
            ("name", r#"{"name":"b2","inputSchema":{"type":"object","properties":{"p":2,"q":1}}}"#),
            ("inputSchema", r#"{"name":"b","inputSchema":{"type":"string","properties":{"p":2,"q":1}}}"#),
            (
                "annotations",
                r#"{"name":"b","inputSchema":{"type":"object","properties":{"p":2,"q":1}},"annotations":{"readOnlyHint":true}}"#,
            ),
            (
                "description",
                r#"{"name":"b","inputSchema":{"type":"object","properties":{"p":2,"q":1}},"description":"new"}"#,
            ),
            // The key-reordering case P0-02's exit criterion names, on a later page.
            // Reordered *inside* , which is where the pin is byte-sensitive:
            // the per-tool preimage hashes each field's own bytes, so reordering the
            // tool object's own top-level keys is correctly invisible (unchanged from
            // P0-02, and the reason this case names the schema specifically).
            (
                "key order inside inputSchema",
                r#"{"name":"b","inputSchema":{"properties":{"q":1,"p":2},"type":"object"}}"#,
            ),
        ] {
            let after = pages(&[first, changed]);
            assert_ne!(
                pin_pages(&before).expect("pin").1,
                pin_pages(&after).expect("pin").1,
                "a change to {label} on page two must move the server pin"
            );
        }
    }

    /// Order-sensitivity survives pagination: the same three tools in a different order
    /// across the same page split pin differently, exactly as they do within one page.
    #[test]
    fn reordering_tools_across_pages_changes_the_pin() {
        let a = r#"{"name":"a","inputSchema":{}}"#;
        let b = r#"{"name":"b","inputSchema":{}}"#;
        let forward = pages(&[a, b]);
        let reversed = pages(&[b, a]);
        assert_ne!(pin_pages(&forward).expect("pin").1, pin_pages(&reversed).expect("pin").1);
    }

    /// **Also-fix 2.** Four tools, the same 2×2 split, the two pages served in the other
    /// order. The primary pin moves — order-sensitivity is deliberate and stays — while the
    /// per-tool pin multiset is identical, so the secondary unordered pin must *not* move.
    /// That pair is what lets a reader tell "reordered" from "changed"; the final assertion
    /// checks the secondary still moves on a real change, or it would distinguish nothing.
    #[test]
    fn whole_page_reordering_moves_only_the_primary_pin() {
        let (a, b, c, d) = (
            r#"{"name":"a","inputSchema":{}}"#,
            r#"{"name":"b","inputSchema":{}}"#,
            r#"{"name":"c","inputSchema":{}}"#,
            r#"{"name":"d","inputSchema":{}}"#,
        );
        let ab_cd = pages(&[&format!("{a},{b}"), &format!("{c},{d}")]);
        let cd_ab = pages(&[&format!("{c},{d}"), &format!("{a},{b}")]);
        let (pins_1, primary_1) = pin_pages(&ab_cd).expect("pin");
        let (pins_2, primary_2) = pin_pages(&cd_ab).expect("pin");

        assert_ne!(primary_1, primary_2, "the primary pin stays order-sensitive");
        assert_eq!(unordered_server_pin_of(&pins_1), unordered_server_pin_of(&pins_2));
        assert_ne!(
            unordered_server_pin_of(&pins_1),
            primary_1,
            "domain separation: the secondary never equals a primary over the same tools"
        );

        let changed = pages(&[&format!("{c},{d}"), &format!("{a},{}", r#"{"name":"b","inputSchema":{"type":"object"}}"#)]);
        let (pins_3, _) = pin_pages(&changed).expect("pin");
        assert_ne!(unordered_server_pin_of(&pins_1), unordered_server_pin_of(&pins_3));
    }

    /// Streaming [`pin_page`] page by page and closing with [`server_pin_of`] must reproduce
    /// [`pin_pages`] exactly — the derivation step relies on it to hold one page at a time.
    #[test]
    fn streaming_pin_page_reproduces_pin_pages() {
        let walk = pages(&[r#"{"name":"a","inputSchema":{}}"#, "", r#"{"name":"b","inputSchema":{}}"#]);
        let (whole_pins, whole) = pin_pages(&walk).expect("pin");
        let streamed: Vec<ToolPin> =
            walk.iter().flat_map(|p| pin_page(p).expect("pin")).collect();
        assert_eq!(server_pin_of(&streamed), whole);
        assert_eq!(streamed.len(), whole_pins.len());
    }

    /// A walk declaring no tools pins to `sha256("")` whatever its page count — the same
    /// value for one empty page and forty. Recorded as a decision, not a defect: see
    /// [`pin_pages`]'s "A walk of zero tools".
    #[test]
    fn a_zero_tool_walk_pins_to_the_empty_preimage_whatever_its_page_count() {
        let one = pin_pages(&pages(&[""])).expect("pin").1;
        let forty = pin_pages(&pages(&[""; 40])).expect("pin").1;
        assert_eq!(one, forty);
        assert_eq!(one, digest_of(b""));
    }

    /// `pin_tools` must keep producing the digest it produced before P0-13, or every pin
    /// already published (and `results/census/pin_stability.json`) stops being comparable.
    ///
    /// Asserted against a **literal digest**, not against `pin_pages` of the same bytes: the
    /// two share an implementation, so comparing them could only prove they agree with each
    /// other, never that either still agrees with July.
    ///
    /// The two literals below were computed independently, from the algorithm as this
    /// module's own doc comments describe it — per field, `sha256` of a presence marker byte
    /// followed by the field's exact received bytes; per tool, `sha256` of those four field
    /// digests concatenated in `(name, inputSchema, annotations, description)` order; per
    /// server, `sha256` of the tool pins concatenated — rather than pasted out of a failing
    /// run of the new code, which would have pinned whatever the new code does, regression
    /// included.
    #[test]
    fn a_single_page_pin_is_byte_for_byte_what_it_was_before_pagination() {
        let raw = tools_list(
            r#"{"name":"read_file","inputSchema":{"type":"object"},"description":"reads a file"}"#,
        );
        let (tools, server) = pin_tools(&raw).expect("pin");
        assert_eq!(
            tools[0].pin.to_string(),
            "418ca9d00c8b018df22049efd8fcd51c5c5333171056c01ffa047cd2abcf9da1"
        );
        assert_eq!(
            server.to_string(),
            "c2164f3c694a659687296b536be3fe1450c11ba33ddd120908ca07628450c2fb"
        );
    }

    /// A pin over no response at all would be a valid-looking digest of nothing, which is
    /// exactly the kind of false-confidence artifact `unverifiable` exists to prevent one
    /// layer up.
    #[test]
    fn pin_pages_rejects_an_empty_walk() {
        let empty: [&[u8]; 0] = [];
        let err = pin_pages(&empty).expect_err("must reject");
        assert!(matches!(err, PinError::Malformed(_)));
    }

    /// A walk is pinned whole or not at all. A partial pin over the pages that happened to
    /// parse would attest to a tool set no server ever returned.
    #[test]
    fn one_malformed_page_fails_the_whole_walk_rather_than_pinning_the_rest() {
        let good = tools_list(r#"{"name":"a","inputSchema":{}}"#);
        let bad = b"not json at all".to_vec();
        assert!(pin_pages(&[good.clone(), bad.clone()]).is_err(), "bad page last");
        assert!(pin_pages(&[bad, good]).is_err(), "bad page first");
    }

}
