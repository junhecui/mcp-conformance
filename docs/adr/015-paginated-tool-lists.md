# ADR-015: Pinning and persisting a paginated `tools/list`

**Status:** Accepted
**Date:** 2026-10-13
**Author:** Jun Cui
**Closes:** [P0-13](../tasks.md#p0-13-follow-toolslist-pagination)
**Related:** [P0-02](../tasks.md#p0-02--metadata-pinner) (the pin this changes the preimage
of), [P0-10](../tasks.md#p0-10-census-evidence-persistence-server-weighted-metrics-offline-re-derivation)
(the one-derivation-path property), [P0-11](../tasks.md#p0-11-correct-dual-era-discovery-for-mcp-2026-07-28)
and [ADR-013](013-era-negotiation.md) (revision-independence, which this must not break),
[P0-12](../tasks.md#p0-12-intra-host-pacing-and-an-etld1-request-budget) and
[ADR-014](014-request-pacing-and-domain-budget.md) (the per-domain budget pagination spends
from), [O-01](../tasks.md#o-01-track-the-tool-annotations-interest-group) (which flagged
this), design.md §3 and §8, architecture.md §6

---

## Context

`ListToolsResult` is a `PaginatedResult` in **both** protocol eras — pagination predates
`2026-07-28` — and this harness has never followed `nextCursor`. Every tool past page one has
therefore been absent from every census number published, and *how many servers paginate was
never measured*, so the size of the loss is unknown rather than known to be zero.

Following the cursor is a loop. The decisions worth recording are the three things the loop
collides with, each of which is an invariant something downstream already depends on:

1. **The pin (P0-02).** It hashes one response's bytes. With N pages there are N responses,
   and the pin has to cover every tool while staying stable against a server that splits the
   same tools differently — *and* still moving when a tool changes.
2. **The evidence (P0-10).** One `tools/list` response was one blob, and every published
   number is re-derived from stored bytes. With N pages there are N blobs, and
   `census-rederive` must still reproduce a live run byte-for-byte.
3. **The budget (ADR-014).** `MAX_REQUESTS_PER_DISCOVERY` is 5 and the per-registrable-domain
   cap of 120 was derived from it. Pagination adds an unknown number of requests, which
   P0-12 anticipated by publishing `overshoot_requests` rather than guessing a larger
   reservation — and which its own limitations list said whoever lands pagination must
   revisit.

A fourth concern is not a collision but the premise: the cursor is a value a server chooses
and this client echoes back. design.md §3 assumes the server is actively hostile.

---

## Decision

### 1. The pin's preimage is the per-tool pins across the whole walk; page framing is excluded

`pin_pages(pages)` hashes every tool of every page, in walk order:

```
server_pin = sha256(tool_pin_0 || tool_pin_1 || … || tool_pin_n)
```

The per-tool preimage is **unchanged** from P0-02 —
`sha256(sha256(0x01‖name) ‖ sha256(0x01‖inputSchema) ‖ sha256(annotations) ‖ sha256(description))`
— and nothing about a page enters the server preimage: not `nextCursor`, not a modern page's
required `resultType`/`ttlMs`/`cacheScope`, not the page boundaries.

`pin_tools(one_response)` remains, and is literally `pin_pages(&[one_response])` — the same
code, not an equivalent one — so a single-page server's pin is **byte-identical** to what it
was before this change.

**Options considered.**

| Option | Same tools, different split | Tool changed | Revision-independent | Single-page pin unchanged |
|---|---|---|---|---|
| A: hash a canonical concatenation of page *bytes* | ✗ pin moves | ✓ | ✗ | ✗ |
| B: hash each page's bytes, then hash those digests | ✗ pin moves | ✓ | ✗ | ✗ |
| **C: hash the per-tool pins, drop page framing** | **✓ pin is stable** | **✓** | **✓** | **✓** |
| D: sort the tool pins before hashing | ✓ | ✓ | ✓ | ✗ (and loses order sensitivity) |

A and B fail on the first column, which is the one that matters most: page size is a server's
(or a gateway's) implementation detail, and a server that moves from one page of six tools to
two pages of three has changed nothing about what it declares. Under A or B it would look
rug-pulled, and at corpus scale that would show up as a wave of spurious pin changes
indistinguishable from the attack the pin exists to detect — which is worse than no pin,
because it trains the reader to ignore it. They also break ADR-013's revision-independence,
since a modern page carries three required result fields a legacy page does not.

D was rejected because order sensitivity is a deliberate P0-02 property ("a server that
reorders its tool list has changed its response") and because it would change every pin
already published for no benefit this change needs.

**The property is proven in both directions, and each direction's test is what stops the
other passing vacuously.**

- *Stable across splits*:
  `the_same_tools_split_across_pages_differently_pin_identically` pins three tools as one
  page, as three pages, as two uneven pages, and as three pages with an empty page in the
  middle, and asserts all four agree per-tool **and** server-wide.
  `a_pin_survives_both_the_revision_and_the_page_split_at_once` (`tests/era_pin.rs`) does the
  harder composition — one legacy page versus two *modern* pages with a cursor between them,
  which differ in three independent ways at once.
- *Still moves on a change*: `a_change_inside_a_tool_on_a_later_page_still_moves_the_pin`
  changes `name`, `inputSchema`, `annotations`, `description`, and the key order *inside*
  `inputSchema`, each on page **two**, and asserts the pin moves every time.
  `a_change_on_a_later_modern_page_still_moves_the_pin` does the same for an annotation flip
  on a later modern page. `reordering_tools_across_pages_changes_the_pin` keeps order
  sensitivity honest across a page boundary.
- *July comparability*: `a_single_page_pin_is_byte_for_byte_what_it_was_before_pagination`
  asserts two literal digests, computed independently from the algorithm as the module
  documents it rather than pasted out of a run of the new code — which would have pinned
  whatever the new code does, regression included.

A walk is pinned whole or not at all: `pin_pages` rejects an empty page list (a pin over no
response would be a valid-looking digest of nothing) and fails if any page is malformed,
because a partial pin would attest to a tool set no server ever returned. A walk whose pages
declare **zero tools** is different — a server declaring an empty tool set — and pins to
`sha256("")` whatever its page count; that is the split-stability property working, not a
digest of nothing, and is pinned by
`a_zero_tool_walk_pins_to_the_empty_preimage_whatever_its_page_count`.

**A secondary, order-insensitive pin is published beside the primary (review fix).** The
option table above tested "same tools, different split" only in one global order. Across
pages, order is far more plausibly a sharding or gateway artifact than a declaration: four
tools served as the same two pages in the other order moved the primary pin while the
per-tool pin multiset was identical — a false-positive rug pull, the failure this ADR calls
worse than no pin, arriving through a door it did not check. Sorting the primary stays
rejected (option D), so a record now carries `server_pin_unordered` — the per-tool pins
hashed in sorted order under a domain-separation prefix — beside `server_pin`. Primary moved
and secondary did not means **reordered**; both moved means **changed**.
`whole_page_reordering_moves_only_the_primary_pin` proves both halves, and that the secondary
still moves on a real change.

### 2. One evidence blob per page, addressed as it arrived; the stop reason is an observation

Each page is `put` into F-05's store as its own blob, in walk order. **Not** spliced into one
artifact: the store is content-addressed over bytes *as received*, and concatenating N
responses would be a transformation at capture time, which ADR-005 puts on the far side of
the normaliser. Content addressing also makes the extra blobs nearly free when two servers
(or one server twice) return an identical page.

A server record carries:

- `evidence.tools_list_raw` — **unchanged in meaning**: page one's digest and byte count, so
  every pre-P0-13 reader of that key sees exactly what it always saw.
- `evidence.tools_list_pages` — the authoritative ordered walk, `{digest, bytes}` per page,
  **always present**, even for a one-page server. An absent array and an array of one are
  different claims about a file and only one of them is checkable.
- `pagination` — `pages_fetched`, `cursors_exhausted`, `stopped_because`,
  `truncation_attribution`, `tools_total` (every occurrence, as received), `tools_distinct`
  (the census tool count), `tools_beyond_first_page`, `duplicate_tool_names`,
  `conflicting_tool_definitions`, `bytes_total`; and beside it `nonconformance_flag`
  (§4).

**`cursors_exhausted`, not `complete` (review fix).** The field asserts that the server
stopped offering cursors — never that the harness saw the server's whole catalogue. A server
that returns a curated subset and then stops offering a cursor reads `true` with no
mechanical signal against it. The stronger-sounding name was the trap P0-11 renamed
`initialize_raw` to remove; the Rust method is renamed with it.

**Derivation streams the walk and re-applies the capture bounds (review fix, B4).** The
store is content-addressed, so one blob referenced N times keeps an evidence bundle tiny
while a derivation that holds every reference resident is not: a 22 KB results file and a
922 KB store measured 184 MB resident, and `census-rederive` accepted it, reporting 200 pages
against a capture bound of 40 — extrapolating to ~90 GB from a 1 MB file and a 10 MiB store.
That attacks this project's own reproducibility claim (ADR-005: a reviewer can be handed the
bundle). Now a `tools_list_pages` array longer than `MAX_PAGES`, or declaring more bytes
than `MAX_TOTAL_BYTES` + one page, is refused as a malformed record before any blob is read;
derivation reads, classifies and pins **one page at a time** (`pin_page` + `server_pin_of`,
which `pin_pages` is literally built from), so peak memory is one page plus the per-tool
pins; it re-measures every blob against the same byte ceiling; and a walk recorded
`no_cursor` with more than `MAX_TOOLS` tools is refused, since no capture can produce one.
`a_crafted_bundle_cannot_amplify_rederivation`. One blob that is itself huge is still read
whole — pre-existing, not amplified, and bounded by whatever the store holds.

Everything in that block **except `stopped_because`** is recomputed from the stored pages at
derivation time, so P0-10's single derivation path is intact. `stopped_because` is carried
through verbatim — the treatment `discovery_path` and the per-server pacing block already get
— because two of its ten values (`budget_exhausted`, `page_request_failed`) are facts about
the *run* rather than about any stored page, and no amount of re-reading the evidence could
recover them.

`a_multi_page_results_file_rederives_byte_for_byte` proves the central claim over a file
containing a three-page walk, a one-page server **and** a truncated walk;
`a_missing_later_page_blob_fails_rederivation_loudly` proves a missing page fails loudly
naming its digest, so "the evidence no longer reproduces the file" can never be mistaken for
"that server had fewer pages".

**One disclosed cost.** A results file written before cursor following has no
`tools_list_pages` and no `pagination`. It re-derives correctly — its one response is read as
one page — but its walk is recorded `unrecorded`, which is deliberately **not** treated as
complete, so its servers come back reclassified as truncated and the file no longer
reproduces byte-for-byte. Calling an unknown walk "complete" would be the same substitution
this project refuses when it insists on `unverifiable` over `holds`. The cost is acceptable
because no file of that shape exists in `results/`: the committed July data predates evidence
persistence entirely and is already refused by `census-rederive`.

### 3. The reservation stays 5, and a walk buys each extra page before sending it

`MAX_REQUESTS_PER_DISCOVERY` is **unchanged**. A cursor walk calls `Permit::try_extend(1)`
immediately before requesting each page beyond the first; the extension is granted only if
the registrable domain's budget has room, and a refusal stops the walk with
`StopReason::BudgetExhausted`.

**Why not raise the reservation.** At the cap of 120 a reservation of 5 admits at least 24
sequences per registrable domain, which is what lets every group in the committed July corpus
except the one 149-entry outlier be attempted in full — the largest legitimate group holds 22
entries (`the_default_cap_admits_every_other_july_group_in_full`). A reservation sized for a
worst-case 40-page walk is 44, which admits **two** sequences per domain and would refuse 20
of those 22 entries. That destroys far more data than pagination recovers, and it would do so
on every domain in the corpus to insure against a page count almost no server has. Raising
the *cap* instead would discard the one number ADR-014 derived from the data.

**Why not simply let the walk overshoot.** That is what P0-12 shipped as a stopgap, and it
makes the cap soft in exactly the way a reservation exists to prevent: the extra pages are
charged against headroom reserved for *later hosts on the same domain*, and
`overshoot_requests` discloses it only after the requests have gone out. Buying ahead keeps
the cap hard and turns "the budget ran out" into a decidable branch at the one moment it can
still be acted on — before the request leaves. `overshoot_requests` consequently stays zero
and recovers its original meaning: a non-zero value is an accounting bug, not "a server
paginated".

**What buying ahead does *not* do, on its own — corrected.** An earlier draft of this
section said it "charges the extra consumption to the domain that caused it rather than
taken from later hosts on it". That is the opposite of what an unbounded extension does:
charging it to the *domain* **is** taking it from later hosts on that domain, because they
are admitted from the same pool. `Permit::try_extend` drew from that shared pool with no
per-sequence accounting, so a 40-page walk took 41 of 120 against an honest sequence's 5 —
8.2× — and on one shared domain the honest co-tenants admitted measured 24 → 15 with one
hostile listing, 8 with two, 2 with four. The victims were recorded
`registrable_domain_budget_exhausted` — attributed to this harness's own politeness control,
indistinguishable from "this domain had more entries than the cap" — while the listing that
displaced them recorded `success`. The grouping key makes it broad: it is a last-two-labels
heuristic, so shared platforms collapse (`alice.vercel.app` and `bob.vercel.app` both key to
`vercel.app`), and the committed corpus already holds a 149-entry and a 22-entry group on
such keys. This ADR rejected a 44-request reservation because it "would refuse 20 of those
22 entries" — and an unbounded extension handed a hostile server that effect on demand.

**Fix: a per-sequence share.** One sequence may hold at most
`cap / MAX_SEQUENCE_SHARE_DIVISOR` = `120 / 8` = **15** requests of its domain, extensions
included (never less than its own admission reservation, so Track B's larger probe
reservation is untouched). That is the 5-request handshake plus ten bought pages — eleven
pages, still an order of magnitude above any tool list observed — and no sequence can hold
more than 12.5% of a domain. The same measurement becomes **24 → 21 → 18 → 12**
(`one_paginating_listing_cannot_starve_its_co_tenants_out_of_the_census`). A walk that hits
its share stops with `budget_exhausted`. The displacement is now published rather than
inferred: each `pacing.registrable_domains[]` row carries `page_extensions_granted`,
`page_extensions_denied`, `page_extensions_denied_by_sequence_share` and
`largest_sequence_reservation`, and the header carries `max_reservation_per_sequence`.
*Considered and not built:* granting an extension only from headroom left after the sweep's
pending candidates on that domain. Tighter, but it needs the pacer to know the corpus, which
it deliberately does not, and the share closes the measured attack structurally.

The reservation is released in full on drop, extensions included, so a long walk cannot
strand headroom for the rest of the run.

A refused extension is counted as `pacing.page_extensions_denied`, **never** pooled into
`sequences_denied_by_budget`: a refused sequence means a host was never contacted, while a
refused page means a host was contacted and its tool list came back truncated. The first is
an absence of coverage numbers, the second a caveat on them.
`verify_pacing_self_consistent` checks `page_extensions_denied` as a **lower bound** on the
server records whose `pagination.stopped_because` is `budget_exhausted` — not an equality,
which falsely rejected two correct shapes (a refusal followed by `evidence_store_error`, whose
record has no `pagination` block; and `census-pin-stability`, which runs two walks under one
permit and writes no block at all) — and derives `overshoot_requests` from each record's own
published `pacing.reservation` rather than from the header constant.

**What it costs a sweep.** Pacing applies per request, so each page is spaced at the 500 ms
default, and the per-sequence share caps one walk at eleven pages on a paced transport.

**None of this applies to Class A (corrected).** Stage 2 discovers over containerised stdio,
built with `stdio_with_timeout` and **no `Pacer`**, so `Transport::try_extend_budget`'s
default grants every page unconditionally: a Class A walk has no budget gate and no spacing,
and a hostile package can walk all 40 pages at pipe speed inside the watchdog.
`budget_exhausted` and `page_extensions_denied` are therefore structurally always zero for
Stage 2. For Class A the page bound is purely a memory control, not a politeness one.

### 4. Four bounds, one cycle check and two failure branches — each a distinguishable outcome

| Dimension | Bound | `stopped_because` |
|---|---|---|
| Pages | `MAX_PAGES` = 40 | `page_limit` |
| Tools across the walk — **after every page, cursor or not** | `MAX_TOOLS` = 5,000 | `tool_limit` |
| Bytes across the walk | `MAX_TOTAL_BYTES` = 32 MiB | `byte_limit` |
| Cursor length | `MAX_CURSOR_BYTES` = 1,024 | `cursor_too_long` |
| Cursor type | must be a JSON string | `cursor_not_a_string` |
| Cursor value | must be non-empty | `cursor_empty` |
| Cursor repetition | a cursor already requested | `cursor_repeated` |
| Domain budget | ADR-014's cap | `budget_exhausted` |
| A later page's request | — | `page_request_failed` |
| Ran out of cursors | — | `no_cursor` — **the only complete outcome** |

**The tool ceiling is absolute (review fix, B1).** It is checked after every page, before the
cursor is looked at. Previously it sat on the continuation side with the others, so a page
offering *no* cursor was never tool-bounded: one page of 20,000 tools (4× the bound, 1.4 MB)
was recorded `success`, and at the 10 MiB page cap one response can carry ~150,000 tools
against July's whole corpus of 3,183 — one server could rewrite every tool-weighted
percentage the census publishes. It is now `tool_limit` however the walk ended; exactly
`MAX_TOOLS` with no cursor remains usable
(`a_single_page_over_the_tool_bound_is_truncated_even_without_a_cursor`). Where a page is
both over the ceiling and carries an unusable cursor, `tool_limit` wins: what was received
is disqualifying on its own.

**Every other bound is checked only when a cursor was actually offered**, so a walk that
reaches exactly 40 pages and whose last page carries no cursor ran out of cursors; it is not
page-limited. Filing a finished observation as truncated would discard a server's coverage
numbers for no reason — the mirror image of the failure this task exists to prevent.
`a_walk_ending_exactly_at_the_page_bound_without_a_cursor_is_not_truncated` exists so that
hoisting these checks to the top of the loop fails loudly.

**A truncated walk is recorded as a failure, not a flagged success.** Its record carries
a `failure_category: "tools_list_truncated_*"` (partitioned by attribution, below) with its pages still in the evidence store and its
`pagination` block saying how far it got, but it contributes no tally, no tool count and no
pin. A flag would not have been enough: the tool-weighted corpus tally would silently gain a
*prefix* of a server instead of a server, and the server-weighted `explicit.on_every_tool`
would report "every tool" over a subset — a strictly **more favourable** claim than the
evidence supports. design.md §8's rule for an unobservable effect is `unverifiable`, never
`holds`; this is the same rule for a partially observed tool list.

**Truncation is partitioned by who caused it, and most of it is not subtractable (review
fix, B3).** An earlier draft called the category "harness-side and therefore subtractable".
That is wrong for seven of the ten reasons, and it was an instruction to a future analyst to
remove evaders from the denominator. A server escapes measurement for the price of one key —
`"nextCursor":""`, a non-string, 1,025 bytes, or a repeat — on a response it was already
sending: two requests, a fully conformant tool list, and before this fix it landed in a
pooled bucket with no tally, count or pin. Before P0-13 the only exit was not answering
`tools/list`, which filed a server as visibly broken; this one was quiet. And the bias is
adverse-selected: the servers with a motive to dodge are the ones that *declare*
annotations (an absent one cannot be violated), so excluding them biases `absent` upward —
spuriously strengthening O-01's "absence rather than mismatch" reading — and will bias the
eventual mismatch rate downward. Both flatter the ecosystem, and both flatter this project's
conclusion.

| Attribution | Stop reasons | Failure category | Subtractable? |
|---|---|---|---|
| `server_nonconformant` | `cursor_empty`, `cursor_not_a_string`, `cursor_too_long`, `cursor_repeated` | `tools_list_truncated_by_server` | **No** — the server's doing |
| `harness_bound` | `page_limit`, `tool_limit`, `byte_limit` | `tools_list_truncated_by_bound` | Yes |
| `harness_budget` | `budget_exhausted` | `tools_list_truncated_by_budget` | Yes |
| `ambiguous` | `page_request_failed` | `tools_list_truncated_ambiguous` | Cannot be attributed |
| `unrecorded` | (pre-P0-13 file) | `tools_list_truncated_unrecorded` | n/a |

Each record carries `pagination.truncation_attribution`; the header publishes
`pagination.servers_truncated_by` seeded with every bucket; `failure_categories` carries the
partition itself; and `finish`'s stderr line breaks it out. A record whose walk was truncated
by its **own server** carries `nonconformance_flag: true` and `nonconformance_reason`
(`false` on every other discovered record, so absence is never read as clear), counted as
`pagination.servers_nonconformance_flagged`. This is the census-pipeline instance of
architecture.md §5.1's pattern — accept the evidence, flag it — which P4-03 carries to
publication; a deliberately malformed cursor in the one field the harness echoes back is
exactly the class of input that pattern exists for. Track B uses the same categories
(`probe_stage1.rs`). `a_server_truncated_walk_is_partitioned_and_flagged`.

`cursor_not_a_string` also covers a JSON string containing a lone unpaired surrogate escape,
which is not representable as a Rust `String`. The wire name is kept; the breadth is
documented on the variant and pinned by a test.

The first page's failure is **not** `page_request_failed`: it fails discovery outright, as it
did before pagination, which also keeps `ModernWithoutDiscover`'s correction path reachable
(that path is triggered by precisely that error).

**What is deliberately not gated.** A cursor containing control characters or unusual Unicode
is accepted. It rides in the next request as a `Value::String`, so `serde_json` escapes it:
it cannot close the enclosing string, inject a member, or smuggle a raw newline into stdio's
one-message-per-line framing. The structural attack is unreachable, and rejecting
odd-looking-but-legal opaque tokens would refuse conformant servers for no gain. Asserted on
the encoded bytes by `a_hostile_cursor_cannot_break_the_request_it_rides_in`. The cursor is
never published and never logged; only its length and the stop reason it produced leave the
crate, and the page it arrived on is in the evidence store like any other response.

**A tool set that changes mid-walk** is a limitation, not a bound. The protocol gives a cursor
walk no snapshot isolation, so a server may legitimately re-list or drop a tool between pages,
and no mechanical check can separate that from a rug pull performed during the walk. What the
harness does: the pin attests to the concatenation actually observed, and `pages_fetched`
makes the window visible.

**Re-listed tools are counted once (review fix, B1).** The derivation used to sum per-page
tallies with no de-duplication: forty pages of the same two tools published as
`tool_count = 80`, `tools_discovered = 80`, `readOnlyHint.explicit = 80` — two real tools as
eighty, one annotation inflated 40× — as a clean success. Now the census tool count and every
tally are over the **name-deduplicated** set, first sighting in walk order winning, and the
raw occurrence count is published beside it (`tools_total` vs `tools_distinct`,
`duplicate_tool_names`). *Why de-duplicate rather than refuse a walk with any repeat:* a
repeat is legitimate under the protocol's no-snapshot-isolation semantics, and refusing it
would hand every server a second one-key exit from the census — the B3 defect again. The pin
is **not** de-duplicated: it stays over what was received.
`forty_pages_of_the_same_two_tools_publish_as_two_tools`.

**`duplicate_tool_names` is one-sided, and the earlier wording implied symmetry.** It catches
a tool re-listed across pages. It does **not** catch a tool *dropped* between pages, and a
server that never repeats a name avoids it entirely. The case it could not separate from a
harmless re-listing — the same name with a *different* definition — is now counted directly
as `conflicting_tool_definitions` (names with more than one distinct per-tool pin;
`a_tool_redefined_mid_walk_is_tallied_once_and_published_as_a_conflict`). A dropped tool
remains undetectable from a single walk.

### 5. The results file publishes what was previously missing, rather than silently correcting it

Counts will rise for paginating servers. That is the fix working, and a reader comparing
against July needs the size of the correction, not a corrected number. So a top-level
`pagination` block carries `servers`, `servers_paginating` (and its percentage),
`pages_fetched_total`, `max_pages_fetched`, `tools_observed_total`,
**`tools_beyond_first_page`** (and its percentage), `servers_truncated`,
`servers_truncated_by` (§4's partition), `servers_nonconformance_flagged`,
`duplicate_tool_names`, `conflicting_tool_definitions`, a `stopped_because` distribution
seeded with every reason plus `unrecorded`, and the `bounds` in force — because `page_limit` means nothing without
them and a reader cannot otherwise reproduce a truncation decision.

`tools_beyond_first_page` is exactly the quantity July was missing. A zero there is a finding
in its own right: it answers O-01's "how many servers paginate is **UNVERIFIED**" with
"none, measurably", which is not a null result.

`tools_observed_total` is deliberately a different number from `tools_discovered`: it includes
truncated servers, which `tools_discovered` excludes because they are recorded as failures.
Both are published so neither has to be inferred.

---

## Consequences

- A pin now attests to a server's whole declared tool list rather than its first page. Every
  pin published before this change was the page-one pin; for a single-page server those are
  the same value (asserted against literal digests), and for a paginating server they are
  not, which is the defect rather than a regression.
- Census counts for paginating servers rise, and the rise is quantified per server and
  corpus-wide instead of being applied silently.
- A sweep sends more requests to servers that paginate, spaced like every other request, and
  bounded by a page cap that is also a politeness control. The per-domain budget and its
  published headline number are unchanged, and the cap is still hard.
- Some servers will produce **no** coverage numbers where before they would have produced
  numbers over page one. That fraction is published (`servers_truncated`,
  `servers_truncated_by`, the `tools_list_truncated_*` categories) and is the same kind of honest no-verdict metric
  ADR-004 predicts for the integrity gate.
- A pre-P0-13 results file re-derives correctly but not byte-identically, and its servers
  come back as truncated rather than asserted complete. No such file exists in `results/`.

## Flagged, recorded rather than fixed

- **The four bounds are choices, not measurements.** 40 pages, 5,000 tools, 32 MiB and 1 KiB
  are sized generously against the largest tool counts this project has observed (112 tools on
  one server, P0-06; ~13 mean, October) but no sweep has yet exercised any of them against a
  real server. The first ≥1,000-host sweep's `stopped_because` distribution is the data that
  should set them, and they are published in the results header so a change to them is a
  visible version bump rather than a silent redefinition.
- **A server can decline to be measured, and the census cannot stop it — only see it.** §4's
  partition and flag make an unfollowable-cursor opt-out *visible*; they do not recover the
  server's numbers. This is its own limitation (design.md §8, P5-05), deliberately **not**
  folded into "observation evasion", which is about a sandboxed tool detecting observation:
  this is a server declining measurement altogether, and it affects census *denominators*
  rather than a verdict. **Recorded, not built:** a sensitivity bound is computable offline
  from the retained page-one evidence of every `server_nonconformant` record — "if every such
  server's withheld tools were maximally unfavourable, `absent` would be X instead of Y" —
  which converts an unquantified bias into a published interval.
- **`MAX_TOOLS` is enforced on a count parsed out of each page, so a page whose `result` is
  not a readable `tools/list` shape counts zero tools toward it.** Such a page still counts
  toward `MAX_PAGES` and `MAX_TOTAL_BYTES`, and the derivation step rejects it as a
  `coverage_extraction` failure rather than tallying it, so the bound cannot be used to make
  the walk unbounded — only to make it reach the page or byte bound instead of the tool one.
- **Peak memory per in-flight discovery is the byte bound *plus one page*: ~42 MiB, not 32
  (corrected).** The byte check runs after a page is appended, so a walk can sit just under
  32 MiB and then add one more 10 MiB page. **No HTTP sweep runs concurrently** — Stage 1 is
  sequential and `--jobs` is Class A only — so the concurrent case is **Stage 2**: up to
  ~336 MiB at `--jobs 8`, held in the host `xtask` process, where the container's
  `--memory=256m` does not reach. Stage 2 also has no pacer (§3), so nothing spaces those
  pages. Real tool lists are kilobytes; the bound exists for the hostile case and has not
  been exercised at `--jobs 8`.
- **Cycle detection is over cursors requested, not over page content.** A server that cycles
  through a long series of *distinct* cursors returning the same page each time is caught by
  `MAX_PAGES`, not by the cycle check, and its `duplicate_tool_names` will be large — which is
  the signal, and it is published, but it is not a bound.
- **Track B refuses to probe a server whose walk was truncated** (the same `tools_list_truncated_*` categories,
  `xtask/src/probe_stage1.rs`), and `census-pin-stability` refuses to pin one, rather than
  recording a pin over a prefix. Both are new skip categories whose rate is unmeasured.
- **No sweep was run.** Nothing in `results/` changed. Every number in this ADR is either a
  test assertion or a figure taken from the committed July data.
