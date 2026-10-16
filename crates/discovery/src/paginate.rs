//! `tools/list` cursor following, and every bound that walk runs under (P0-13).
//!
//! `ListToolsResult` is a `PaginatedResult` in **both** protocol eras — pagination predates
//! `2026-07-28` — and this harness never followed `nextCursor` until now, so every tool past
//! page one was silently absent from every census number published. That omission is
//! unmeasured rather than known to be zero, which is why [`PaginationOutcome`] and the
//! per-server block built from it publish *how many* pages were fetched and *how many* tools
//! lay beyond the first.
//!
//! # This module interprets nothing beyond what a bound requires
//!
//! It reads two things out of a page: the continuation cursor, and how many tools the page
//! declares. The second exists only to enforce [`MAX_TOOLS`]. Coverage classification and
//! pinning stay where they are (`census::coverage`, [`crate::pin`]), over the stored bytes.
//!
//! # The cursor is server-controlled, so every dimension is bounded
//!
//! design.md §3 assumes the server is actively hostile, and a cursor is a value the server
//! chooses and this client echoes back in its next request. A walk driven by that value can
//! be made unbounded in four independent directions — pages, tools, bytes, and the length of
//! the cursor itself — and a cursor can be made to cycle. Each bound has its own
//! [`StopReason`], because this project's rule is that a truncated observation must never
//! look like a complete one: a partial tool list silently filed as complete would corrupt
//! every annotation-coverage number computed from it, which is the worst available failure
//! mode for a trust signal (design.md §8).
//!
//! What is deliberately **not** gated: a cursor containing control characters or unusual
//! Unicode. `serde_json` escapes a string when it serialises one, so a cursor cannot break
//! JSON framing or smuggle a second newline-delimited message over stdio no matter what it
//! contains — the structural attack is unreachable, and refusing odd-looking-but-legal
//! opaque tokens would reject conformant servers for no gain. The cursor is never published
//! and never logged; only its length, and the stop reason it produced, leave this crate. The
//! bytes it arrived in stay in the evidence store like every other response.

use serde::Deserialize;
use serde_json::value::RawValue;

/// Most `tools/list` pages one walk will fetch.
///
/// Generous by about two orders of magnitude against observed reality — the largest single
/// server hand-verified in P0-06 declared 112 tools, and the October Class B census averaged
/// ~13 tools per server over 3,183 tools — while bounding the conversation a single host can
/// hold this harness in.
///
/// **What it controls depends on the transport.** Over paced HTTP (Stage 1, Track B) every
/// page is spaced (P0-12) and bought from the registrable domain's budget, so 40 pages at
/// the 500 ms default is a ~20-second conversation and the bound is a politeness control as
/// well as a memory one. Over stdio (Stage 2, Class A) there is **no** pacer: the transport's
/// budget hook grants every page unconditionally and nothing spaces the requests, so a
/// hostile package can drive all 40 pages at pipe speed inside the watchdog. For Class A
/// this bound is purely a memory control, and `budget_exhausted` can never occur there.
pub const MAX_PAGES: usize = 40;

/// Most tools one walk will accumulate across all its pages.
///
/// ~45x the largest tool count this project has ever observed on one server (112, P0-06).
/// Past this a server is either broken or trying to make the derivation step expensive, and
/// either way the observation is not usable as a coverage number.
///
/// **Enforced after every page, unconditionally** — unlike [`MAX_PAGES`], which only stops
/// a walk that is still being offered a cursor. A single page carrying 20,000 tools and no
/// cursor is not a complete observation that happens to be large; it is one server able to
/// outweigh the whole corpus in every tool-weighted percentage. See
/// `DiscoveryClient::fetch_tools_list`.
pub const MAX_TOOLS: usize = 5_000;

/// Most bytes one walk will accumulate before this client stops asking for another page.
///
/// Each individual page is already capped at 10 MiB by both transports, but 40 pages of
/// that would be 400 MiB held at once per in-flight discovery. This bound is checked on the
/// continuation side, after a page has been appended, so **the real per-walk ceiling is
/// just under this plus one page: ~42 MiB**, not 32. At Stage 2's `--jobs 8` (the only
/// sweep that runs discoveries concurrently — Stage 1 is sequential) that is ~336 MiB of
/// page buffers in the hostile case, held in the host `xtask` process, where the
/// container's `--memory` limit does not reach.
pub const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;

/// Largest single response either transport will read — one `tools/list` page at most.
///
/// The one definition both transports' caps are taken from (`transport.rs`), published here
/// so a consumer re-applying the walk ceiling at derivation time — [`MAX_TOTAL_BYTES`] plus
/// one page — uses the same number capture did rather than a copy that can drift.
pub const MAX_PAGE_BYTES: usize = 10 * 1024 * 1024;

/// Longest `nextCursor` this client will echo back.
///
/// The spec makes a cursor an opaque string with no length guidance, so this is a choice:
/// 1 KiB is ample for any real continuation token (the registry cursors elsewhere in this
/// codebase are tens of bytes) while bounding the request body a server can make this client
/// emit, and bounding it at the point the value is *read* rather than at each place it might
/// later be used.
pub const MAX_CURSOR_BYTES: usize = 1024;

/// Why a `tools/list` cursor walk stopped.
///
/// Exactly one variant means the walk ran out of cursors ([`Self::NoCursor`]); every other
/// means it is truncated, and says which bound or failure truncated it. A closed taxonomy,
/// in the shape [`crate::FallbackReason`] already uses for era provenance — so a published
/// distribution can be seeded with explicit zeroes and an absent bucket cannot be read as a
/// zero one. Who is responsible for each truncation is [`Self::attribution`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The last page carried no `nextCursor` (absent, or an explicit JSON `null`). **The
    /// only outcome a coverage number may be computed from** — see
    /// [`Self::cursors_exhausted`] for exactly what it does and does not assert.
    NoCursor,
    /// [`MAX_PAGES`] reached while a cursor was still being offered.
    PageLimit,
    /// [`MAX_TOOLS`] exceeded — checked after every page, whether or not a cursor was
    /// offered — or reached while a cursor was still being offered.
    ToolLimit,
    /// [`MAX_TOTAL_BYTES`] reached while a cursor was still being offered.
    ByteLimit,
    /// A `nextCursor` longer than [`MAX_CURSOR_BYTES`].
    CursorTooLong,
    /// A `nextCursor` this client cannot read back as a string.
    ///
    /// Two shapes land here and the name names only the first: a cursor that is a number,
    /// object, array or boolean where the spec requires a string, **and** a cursor that is
    /// a JSON string whose escape sequences do not decode to valid UTF-8 — a lone unpaired
    /// surrogate escape, which `serde_json` refuses to decode into a Rust `String`. Both
    /// are server nonconformance and both are equally unfollowable, so they share a reason
    /// rather than widening a published taxonomy for a distinction no consumer would act on
    /// differently. The wire string stays `cursor_not_a_string` so a reader's parser does
    /// not move; `a_lone_surrogate_cursor_is_unusable_for_the_same_recorded_reason` pins
    /// the second shape so the breadth is asserted rather than described.
    CursorNotAString,
    /// A `nextCursor` present as an empty string, which names no page and cannot be
    /// followed.
    CursorEmpty,
    /// A `nextCursor` this walk has already requested: the server is cycling.
    CursorRepeated,
    /// The registrable domain's request budget (P0-12) had no room for another page, or
    /// this sequence reached its own share of that budget. The walk is truncated by *this
    /// harness's own* politeness control rather than by the server, which is why it is its
    /// own reason and not pooled with the bounds above.
    BudgetExhausted,
    /// A later page's request failed — transport, HTTP status, or a response that was not a
    /// well-formed JSON-RPC result. The **first** page's failure is never this: it fails
    /// discovery outright, exactly as it did before pagination.
    PageRequestFailed,
}

/// Who or what truncated a walk — the partition every truncated outcome is published under.
///
/// The ten [`StopReason`]s are not one finding. Four of them are a server declining to be
/// measured, three are a bound *this harness* chose, one is this harness's own politeness
/// control, and one cannot be attributed either way. Pooling them into a single
/// "truncated" bucket makes a server that escapes the census by emitting one malformed
/// cursor indistinguishable from a server this harness cut off — and the two have opposite
/// implications for every published denominator, because only the harness-side ones are
/// the harness's to subtract.
///
/// Why that matters more than a taxonomy usually does: the servers with a motive to dodge
/// measurement are exactly the ones that *declare* annotations, since an absent annotation
/// cannot be found violated. A blurred bucket therefore biases the published `absent` share
/// **upward** and the eventual mismatch rate **downward** — both in the direction that
/// flatters the ecosystem, and both in the direction that flatters this project's own
/// "absence rather than mismatch" reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncationAttribution {
    /// The server sent a `nextCursor` that cannot be followed: empty, not a readable
    /// string, over-long, or one already requested. **Not** this harness's doing, and
    /// never subtractable from a denominator: a conformant server does not do this, and a
    /// server with something to hide can do it for the price of one extra JSON key on a
    /// response it was already sending.
    ServerNonconformant,
    /// A bound this harness chose — [`MAX_PAGES`], [`MAX_TOOLS`], [`MAX_TOTAL_BYTES`].
    /// Harness-side: a different bound would have produced an observation.
    HarnessBound,
    /// This harness's own per-domain request budget (P0-12) had no room. Harness-side, and
    /// the one reason a *re-run* with more headroom would decide differently.
    HarnessBudget,
    /// A later page's request failed. Unattributable: a 500 from the server, a dropped
    /// connection, and a transient network fault are indistinguishable at this layer, so
    /// this is its own bucket rather than being guessed into one of the three above.
    Ambiguous,
    /// The walk was not truncated at all — [`StopReason::NoCursor`].
    NotTruncated,
}

impl TruncationAttribution {
    /// Stable, lowercase-snake-case string form, for published provenance fields.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ServerNonconformant => "server_nonconformant",
            Self::HarnessBound => "harness_bound",
            Self::HarnessBudget => "harness_budget",
            Self::Ambiguous => "ambiguous",
            Self::NotTruncated => "not_truncated",
        }
    }

    /// Whether this truncation is the *server's* doing rather than this harness's.
    ///
    /// The one question the nonconformance flag answers, and the reason it is a method
    /// rather than a comparison each caller writes: a record whose walk was truncated by its
    /// own server carries the flag through to publication, in the shape architecture.md
    /// §5.1 established for the sandbox (accept the evidence, flag it) and P4-03 carries to
    /// publication.
    #[must_use]
    pub fn is_server_nonconformance(self) -> bool {
        matches!(self, Self::ServerNonconformant)
    }

    /// Every truncation bucket, so a published distribution can be seeded with explicit
    /// zeroes and an absent bucket can never be read as a zero one.
    ///
    /// [`Self::NotTruncated`] is excluded: it is the complete outcome, and a "truncated by:
    /// not truncated" row would be nonsense in the one table this seeds.
    #[must_use]
    pub fn all_truncating() -> &'static [Self] {
        &[Self::ServerNonconformant, Self::HarnessBound, Self::HarnessBudget, Self::Ambiguous]
    }
}

impl core::fmt::Display for TruncationAttribution {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl StopReason {
    /// Whether this walk ran out of cursors — i.e. the server stopped offering one.
    ///
    /// **Named for exactly what it asserts, and no more.** It says the walk followed
    /// `nextCursor` until no `nextCursor` came back. It does **not** say the harness saw
    /// the server's whole catalogue of tools: a server that returns a curated subset and
    /// then stops offering a cursor is indistinguishable here from one that returned
    /// everything, and no mechanical signal separates them. The earlier name for this
    /// (`is_complete`) claimed the second thing while meaning the first, which is the trap
    /// P0-11 renamed `initialize_raw` to remove and P0-13 removed `tools_list_raw` for.
    ///
    /// It is still the one question every consumer must ask before computing a coverage
    /// number, because a walk that stopped for any *other* reason is a known prefix — which
    /// is why this is a method here rather than a comparison each caller writes.
    #[must_use]
    pub fn cursors_exhausted(self) -> bool {
        matches!(self, Self::NoCursor)
    }

    /// Who truncated a walk that stopped for this reason. See [`TruncationAttribution`].
    #[must_use]
    pub fn attribution(self) -> TruncationAttribution {
        match self {
            Self::NoCursor => TruncationAttribution::NotTruncated,
            Self::CursorEmpty
            | Self::CursorNotAString
            | Self::CursorTooLong
            | Self::CursorRepeated => TruncationAttribution::ServerNonconformant,
            Self::PageLimit | Self::ToolLimit | Self::ByteLimit => {
                TruncationAttribution::HarnessBound
            }
            Self::BudgetExhausted => TruncationAttribution::HarnessBudget,
            Self::PageRequestFailed => TruncationAttribution::Ambiguous,
        }
    }

    /// Stable, lowercase-snake-case string form, for provenance fields in published results.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoCursor => "no_cursor",
            Self::PageLimit => "page_limit",
            Self::ToolLimit => "tool_limit",
            Self::ByteLimit => "byte_limit",
            Self::CursorTooLong => "cursor_too_long",
            Self::CursorNotAString => "cursor_not_a_string",
            Self::CursorEmpty => "cursor_empty",
            Self::CursorRepeated => "cursor_repeated",
            Self::BudgetExhausted => "budget_exhausted",
            Self::PageRequestFailed => "page_request_failed",
        }
    }

    /// The inverse of [`Self::as_str`], for reading a stop reason back out of a results file.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Self::all().iter().copied().find(|r| r.as_str() == s)
    }

    /// Every variant, so a report can seed a distribution with explicit zeroes instead of
    /// omitting the reasons a run happened not to hit.
    #[must_use]
    pub fn all() -> &'static [Self] {
        &[
            Self::NoCursor,
            Self::PageLimit,
            Self::ToolLimit,
            Self::ByteLimit,
            Self::CursorTooLong,
            Self::CursorNotAString,
            Self::CursorEmpty,
            Self::CursorRepeated,
            Self::BudgetExhausted,
            Self::PageRequestFailed,
        ]
    }
}

impl core::fmt::Display for StopReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What one `tools/list` cursor walk did.
///
/// Deliberately small. `pages_fetched` and `stopped_because` are the parts a consumer cannot
/// recompute from the stored page bytes — the stop reason, on its two failure branches, is
/// in no page — so they travel as observations. Tool counts, byte totals and duplicate names
/// are all derivable from the pages themselves and are therefore *not* here: they are
/// recomputed at derivation time, which is what keeps P0-10's single-derivation-path
/// property true for pagination too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaginationOutcome {
    /// Pages actually fetched and kept. At least 1 for any successful discovery.
    pub pages_fetched: usize,
    /// Why the walk stopped. See [`StopReason::cursors_exhausted`].
    pub stopped_because: StopReason,
}

impl PaginationOutcome {
    /// Whether the walk ran out of cursors. See [`StopReason::cursors_exhausted`] for what
    /// that does and does not assert.
    #[must_use]
    pub fn cursors_exhausted(&self) -> bool {
        self.stopped_because.cursors_exhausted()
    }

    /// Who truncated this walk. See [`TruncationAttribution`].
    #[must_use]
    pub fn attribution(&self) -> TruncationAttribution {
        self.stopped_because.attribution()
    }
}

/// What a page says about continuing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Continuation {
    /// No `nextCursor`: the walk is complete.
    Done,
    /// A usable cursor.
    Next(String),
    /// A cursor is present and cannot be followed, for this reason.
    Unusable(StopReason),
}

#[derive(Deserialize)]
struct Envelope<'a> {
    #[serde(borrow)]
    result: &'a RawValue,
}

#[derive(Deserialize)]
struct Paginated<'a> {
    #[serde(borrow, rename = "nextCursor", default)]
    next_cursor: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct ToolsOnly<'a> {
    #[serde(borrow, default)]
    tools: Vec<&'a RawValue>,
}

/// Read one page's continuation cursor, without interpreting anything else in it.
///
/// A page that is not a JSON-RPC result at all is [`Continuation::Done`]: discovery only
/// calls this on a response `decode_and_validate` has already accepted as a well-formed
/// result, and a page whose `result` is malformed beyond that fails loudly downstream
/// (`census::coverage` and [`crate::pin_pages`] both reject it). Guessing a cursor out of a
/// shape this cannot parse would be the one place a malformed page could *extend* a walk.
///
/// An explicit `"nextCursor": null` reads as [`Continuation::Done`], which is correct and is
/// also forced: serde collapses a JSON `null` into `None` for an `Option` before
/// [`RawValue`] ever sees it — the same boundary [`crate::pin`]'s `field_digest` documents.
pub(crate) fn continuation(page: &[u8]) -> Continuation {
    let Ok(envelope) = serde_json::from_slice::<Envelope>(page) else {
        return Continuation::Done;
    };
    let Ok(paginated) = serde_json::from_str::<Paginated>(envelope.result.get()) else {
        return Continuation::Done;
    };
    let Some(raw) = paginated.next_cursor else {
        return Continuation::Done;
    };
    // Parsed as a string rather than string-matched: this is the one server-controlled value
    // this client puts back on the wire, so its *type* is checked before its length.
    let Ok(cursor) = serde_json::from_str::<String>(raw.get()) else {
        return Continuation::Unusable(StopReason::CursorNotAString);
    };
    if cursor.is_empty() {
        return Continuation::Unusable(StopReason::CursorEmpty);
    }
    if cursor.len() > MAX_CURSOR_BYTES {
        return Continuation::Unusable(StopReason::CursorTooLong);
    }
    Continuation::Next(cursor)
}

/// How many tools one page declares, for [`MAX_TOOLS`] accounting only.
///
/// Zero for a page whose `result` is not a readable `tools/list` shape — the bound must not
/// be defeatable by a malformed page, and it is not: such a page still counts toward
/// [`MAX_PAGES`] and [`MAX_TOTAL_BYTES`], and the derivation step rejects it as a
/// `coverage_extraction` failure rather than tallying it.
pub(crate) fn count_tools(page: &[u8]) -> usize {
    serde_json::from_slice::<Envelope>(page)
        .ok()
        .and_then(|envelope| serde_json::from_str::<ToolsOnly>(envelope.result.get()).ok())
        .map_or(0, |result| result.tools.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(result_body: &str) -> Vec<u8> {
        format!(r#"{{"jsonrpc":"2.0","id":1,"result":{result_body}}}"#).into_bytes()
    }

    #[test]
    fn a_page_with_no_next_cursor_ends_the_walk() {
        assert_eq!(continuation(&page(r#"{"tools":[]}"#)), Continuation::Done);
    }

    /// An explicit `null` is the spec's own way of saying "last page" and must read the same
    /// as an absent key. Serde forces this; the test pins that it stays forced.
    #[test]
    fn an_explicit_null_next_cursor_ends_the_walk() {
        assert_eq!(continuation(&page(r#"{"tools":[],"nextCursor":null}"#)), Continuation::Done);
    }

    #[test]
    fn a_usable_cursor_is_returned_verbatim() {
        assert_eq!(
            continuation(&page(r#"{"tools":[],"nextCursor":"page-2"}"#)),
            Continuation::Next("page-2".to_string())
        );
    }

    #[test]
    fn a_non_string_cursor_is_unusable_rather_than_coerced() {
        for body in [
            r#"{"tools":[],"nextCursor":2}"#,
            r#"{"tools":[],"nextCursor":{"n":2}}"#,
            r#"{"tools":[],"nextCursor":["page-2"]}"#,
            r#"{"tools":[],"nextCursor":true}"#,
        ] {
            assert_eq!(
                continuation(&page(body)),
                Continuation::Unusable(StopReason::CursorNotAString),
                "{body}"
            );
        }
    }

    /// A lone unpaired surrogate escape is a legal JSON string that is not a legal Rust
    /// `String`, so it lands in [`StopReason::CursorNotAString`] alongside the non-string
    /// JSON types. Asserted because the variant's *name* only describes the other half, and
    /// a future reader tightening that name needs to see this case first. The escape is
    /// assembled at runtime so no source-level escape sequence exists to be mangled.
    #[test]
    fn a_lone_surrogate_cursor_is_unusable_for_the_same_recorded_reason() {
        let escape = format!("{}u{}", '\\', "d800");
        let body = format!(r#"{{"tools":[],"nextCursor":"{escape}"}}"#);
        assert_eq!(
            continuation(&page(&body)),
            Continuation::Unusable(StopReason::CursorNotAString),
            "an unpaired surrogate is unfollowable, and shares the non-string reason"
        );
    }

    #[test]
    fn an_empty_cursor_is_its_own_reason_and_not_confused_with_absence() {
        assert_eq!(
            continuation(&page(r#"{"tools":[],"nextCursor":""}"#)),
            Continuation::Unusable(StopReason::CursorEmpty)
        );
    }

    #[test]
    fn a_cursor_over_the_length_bound_is_refused_at_the_boundary() {
        let at_bound = "c".repeat(MAX_CURSOR_BYTES);
        assert_eq!(
            continuation(&page(&format!(r#"{{"tools":[],"nextCursor":"{at_bound}"}}"#))),
            Continuation::Next(at_bound),
            "a cursor of exactly the bound is usable"
        );
        let over_bound = "c".repeat(MAX_CURSOR_BYTES + 1);
        assert_eq!(
            continuation(&page(&format!(r#"{{"tools":[],"nextCursor":"{over_bound}"}}"#))),
            Continuation::Unusable(StopReason::CursorTooLong)
        );
    }

    /// A control-character cursor is *accepted*, deliberately: `serde_json` escapes it when
    /// the next request is serialised, so it cannot break JSON framing or smuggle a second
    /// newline-delimited message over stdio. Pinned as a decision rather than left for
    /// someone to "fix" into a rejection that would refuse conformant opaque tokens.
    #[test]
    fn a_control_character_cursor_is_usable_and_cannot_break_framing() {
        let Continuation::Next(cursor) =
            continuation(&page(r#"{"tools":[],"nextCursor":"a\nb\u0000c"}"#))
        else {
            panic!("a control-character cursor is a legal opaque token");
        };
        assert!(cursor.contains('\n'), "the raw cursor really does contain a newline");
        let encoded = crate::jsonrpc::encode_request(
            7,
            "tools/list",
            serde_json::json!({ "cursor": cursor }),
        );
        assert!(
            !encoded.contains(&b'\n'),
            "the encoded request must carry no raw newline, or stdio framing breaks"
        );
    }

    #[test]
    fn an_unparseable_page_never_extends_the_walk() {
        for bytes in [
            &b"not json"[..],
            &br#"{"jsonrpc":"2.0","id":1}"#[..],
            &br#"{"jsonrpc":"2.0","id":1,"result":"a string"}"#[..],
        ] {
            assert_eq!(
                continuation(bytes),
                Continuation::Done,
                "{:?}",
                String::from_utf8_lossy(bytes)
            );
        }
    }

    #[test]
    fn tools_are_counted_per_page_and_a_malformed_page_counts_zero() {
        assert_eq!(count_tools(&page(r#"{"tools":[{"name":"a"},{"name":"b"}]}"#)), 2);
        assert_eq!(count_tools(&page(r#"{"tools":[]}"#)), 0);
        assert_eq!(count_tools(&page(r#"{}"#)), 0);
        assert_eq!(count_tools(b"not json"), 0);
    }

    #[test]
    fn exactly_one_stop_reason_exhausts_its_cursors() {
        let done: Vec<_> = StopReason::all().iter().filter(|r| r.cursors_exhausted()).collect();
        assert_eq!(done, vec![&StopReason::NoCursor]);
    }

    /// The partition every truncated outcome is published under (B3). Asserted
    /// exhaustively over `all()` rather than case by case, so a reason added later without
    /// an attribution fails this test instead of silently defaulting into a bucket.
    #[test]
    fn every_stop_reason_is_attributed_and_only_the_servers_own_are_flagged() {
        use TruncationAttribution as A;
        let expected = [
            (StopReason::NoCursor, A::NotTruncated),
            (StopReason::PageLimit, A::HarnessBound),
            (StopReason::ToolLimit, A::HarnessBound),
            (StopReason::ByteLimit, A::HarnessBound),
            (StopReason::CursorTooLong, A::ServerNonconformant),
            (StopReason::CursorNotAString, A::ServerNonconformant),
            (StopReason::CursorEmpty, A::ServerNonconformant),
            (StopReason::CursorRepeated, A::ServerNonconformant),
            (StopReason::BudgetExhausted, A::HarnessBudget),
            (StopReason::PageRequestFailed, A::Ambiguous),
        ];
        assert_eq!(
            expected.len(),
            StopReason::all().len(),
            "every stop reason needs an attribution, including any added after this test"
        );
        for (reason, attribution) in expected {
            assert_eq!(reason.attribution(), attribution, "{reason}");
            assert_eq!(
                reason.attribution().is_server_nonconformance(),
                attribution == A::ServerNonconformant,
                "{reason}: only a server-caused truncation may set the flag"
            );
            assert_eq!(
                A::all_truncating().contains(&reason.attribution()),
                !reason.cursors_exhausted(),
                "{reason}"
            );
        }
    }

    #[test]
    fn every_stop_reason_round_trips_through_its_string_form() {
        for reason in StopReason::all() {
            assert_eq!(StopReason::parse(reason.as_str()), Some(*reason));
        }
        assert_eq!(StopReason::parse("something_else"), None);
    }
}
